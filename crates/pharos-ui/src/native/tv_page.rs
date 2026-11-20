//! The Android TV app's page rules (spec 025), kept apart from the I/O in
//! `android_page` so host tests cover them: which server the app talks to,
//! the in-app history Back walks, which page requests go to the server, how
//! a login answer reads as the sign-in form's status, and the messages the
//! page's script sends into Rust.

use pharos_hls::planner::Track;
use serde::Deserialize;

use super::hls_trace::{Appended, Clip, Signal};
use super::shell::Shell;

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

/// The query of `path`, `?` and all, without any fragment; empty when it
/// has none, as `location.search` reads (B1276).
pub fn search_of(path: &str) -> &str {
    let path = path.split('#').next().unwrap_or_default();
    path.find('?').map_or("", |at| &path[at..])
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

/// Scheme, host and port of an absolute `http(s)` URL, lower-cased, the
/// port defaulted by scheme, and an IPv6 literal in its one canonical
/// spelling (`[2001:db8::1]` however it was written), so two spellings of
/// one address are one origin. None for anything else.
pub(crate) fn origin_of(url: &str) -> Option<(String, String, u16)> {
    // The same parser the HTTP client sends with (reqwest's `Url`), so an
    // origin compares as the request will reach it: IDNA, percent-encoding,
    // IPv4 shorthands and IPv6 spellings all canonical.
    let parsed = url::Url::parse(url).ok()?;
    let scheme = parsed.scheme().to_owned();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host = parsed.host_str().filter(|h| !h.is_empty())?.to_owned();
    let port = parsed.port_or_known_default()?;
    Some((scheme, host, port))
}

/// The redirects a download follows: the tenth is followed, an eleventh is
/// not (as reqwest's default limit).
pub const DOWNLOAD_REDIRECTS: usize = 10;

/// Whether a redirect with `previous` URLs already requested in its chain
/// (the original among them, as reqwest's `Attempt::previous` counts) is
/// past [`DOWNLOAD_REDIRECTS`].
pub fn redirects_exceeded(previous: usize) -> bool {
    previous > DOWNLOAD_REDIRECTS
}

/// Whether a download's request to `original` may follow a redirect to
/// `next`: only when scheme, host and port all match. The download sends the
/// device token in a header, so a redirect to another host would hand it
/// over, and one from https to http would send it in the clear. Plain http to
/// the same http origin stays allowed: the server may be on the LAN. Err is
/// the reason it stops, for the log.
pub fn redirect_verdict(original: &str, next: &str) -> Result<(), String> {
    let (Some(from), Some(to)) = (origin_of(original), origin_of(next)) else {
        return Err("redirect target is not an http(s) URL".to_owned());
    };
    if from == to {
        return Ok(());
    }
    if from.0 == "https" && to.0 == "http" {
        return Err(format!("https to http downgrade to {}:{}", to.1, to.2));
    }
    if from.1 != to.1 {
        return Err(format!("another host, {}", to.1));
    }
    if from.2 != to.2 {
        return Err(format!("another port, {}", to.2));
    }
    Err(format!("another scheme, {}", to.0))
}

/// Whether the device token may go to `url` in a header: only over https,
/// or to the server the build was pointed at (`PHAROS_TV_SERVER`, passed as
/// `build_server`), whatever its scheme. Any other plain-http request goes
/// without the token. Err is the reason, for the log.
///
/// A build pointed at a plain-http server that is not loopback (a server on
/// the LAN) sends the token to it unencrypted, readable by anyone on that
/// network. That is an accepted risk for LAN builds: the build names its
/// server, and https is the remedy.
pub fn token_allowed(url: &str, build_server: &str) -> Result<(), String> {
    let Some(to) = origin_of(url) else {
        return Err("not an http(s) URL".to_owned());
    };
    if to.0 == "https" {
        return Ok(());
    }
    if origin_of(build_server).as_ref() == Some(&to) {
        return Ok(());
    }
    Err(format!(
        "plain http to {}:{}, not the build's server",
        to.1, to.2
    ))
}

/// What a request carrying the device token does with one redirect hop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hop {
    /// Same origin, where the token may go: follow and keep it.
    FollowWithToken,
    /// Another origin (artwork on another host, say): follow, without the
    /// token.
    FollowWithoutToken,
    /// Not followed, for the reason given (an https-to-http downgrade, a
    /// target that is not http(s)).
    Refuse(String),
}

/// The proxy's decision for one redirect hop from `from` to `to`. The
/// token stays only on the same origin, and there only where it may go at
/// all ([`token_allowed`]: https, or the build's own server); another
/// origin is followed without it, and a downgrade to http is refused.
pub fn redirect_hop(from: &str, to: &str, build_server: &str) -> Hop {
    let (Some(a), Some(b)) = (origin_of(from), origin_of(to)) else {
        return Hop::Refuse("redirect target is not an http(s) URL".to_owned());
    };
    if a.0 == "https" && b.0 == "http" {
        return Hop::Refuse(format!("https to http downgrade to {}:{}", b.1, b.2));
    }
    if a == b && token_allowed(to, build_server).is_ok() {
        return Hop::FollowWithToken;
    }
    Hop::FollowWithoutToken
}

/// The download client, or why there is none. A client that cannot be built
/// is never replaced by a default one: its redirect policy is what keeps the
/// device token on the server's origin, so the download is refused with the
/// error named.
pub fn built_client<C, E: std::fmt::Display>(
    build: impl FnOnce() -> Result<C, E>,
) -> Result<C, String> {
    built_client_for("download", build)
}

/// [`built_client`] for the client named `what` (the proxy's, the
/// download's): one that cannot be built is the error, naming it, never a
/// default client in its place.
pub fn built_client_for<C, E: std::fmt::Display>(
    what: &str,
    build: impl FnOnce() -> Result<C, E>,
) -> Result<C, String> {
    build().map_err(|e| format!("the {what} client could not be built: {e}"))
}

/// One hop of a proxied request: a GET of `url` with `headers`, no redirect
/// followed. The phone's is its reqwest client; a host test's can count
/// what reaches a server.
pub trait ProxyTransport: Sync {
    type Response: ProxyResponse + Send;
    /// Err is the reason, for `stage=send`.
    fn get(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> impl std::future::Future<Output = Result<Self::Response, String>> + Send;
}

/// What one hop answered.
pub trait ProxyResponse {
    fn status(&self) -> u16;
    fn header(&self, name: &str) -> Option<String>;
    /// The redirect target, made absolute against this response's URL, when
    /// the answer is a redirect with a `Location`; Err when it cannot be
    /// read.
    fn redirect(&self) -> Option<Result<String, String>>;
    /// Err is the reason, for `stage=body`.
    fn body(self) -> impl std::future::Future<Output = Result<Vec<u8>, String>> + Send;
}

/// How loud a proxy log line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Debug,
    Info,
    Warn,
}

/// A proxied GET: where it goes and what it may carry.
pub struct ProxyGet<'a> {
    pub url: String,
    /// The device token, when it may go to `url` ([`token_allowed`]).
    pub token: Option<String>,
    pub headers: &'a [(String, String)],
    pub build_server: &'a str,
    /// The response headers passed back.
    pub passed_back: &'a [&'a str],
    pub max_hops: usize,
}

/// Whether the request header `name` may go to another origin
/// ([`CROSS_ORIGIN_HEADERS`]).
fn crosses_origins(name: &str) -> bool {
    CROSS_ORIGIN_HEADERS
        .iter()
        .any(|h| h.eq_ignore_ascii_case(name))
}

/// The proxy's client from `build`: one that cannot be built is the error,
/// naming the proxy, never a default client in its place
/// ([`built_client_for`]).
pub fn proxy_client<C>(build: impl FnOnce() -> Result<C, String>) -> Result<C, String> {
    built_client_for("proxy", build)
}

/// The caller's request headers that may go to another origin than the
/// request's own: content negotiation and validators, nothing that can
/// carry a credential. Any other header (`Authorization`, `Cookie`, ...)
/// is sent only while the request is on its original origin.
pub const CROSS_ORIGIN_HEADERS: &[&str] = &[
    "accept",
    "accept-language",
    "if-modified-since",
    "if-none-match",
    "range",
];

/// Runs a proxied GET over `client`, following redirects by hand through
/// [`redirect_hop`]: the token stays only on its own origin, and at most
/// `max_hops` are followed. With no client, nothing is sent: the request
/// is refused with the reason (`stage=client`). Returns the status, the
/// passed-back headers and the body; `log` gets each decision.
pub async fn proxy_get<T: ProxyTransport>(
    client: &Result<T, String>,
    get: ProxyGet<'_>,
    log: &(dyn Fn(Level, String) + Sync),
) -> Result<(u16, Vec<(String, String)>, Vec<u8>), String> {
    let client = client.as_ref().map_err(|why| {
        log(
            Level::Warn,
            format!("pharos: proxy request refused reason={why}"),
        );
        format!("stage=client {why}")
    })?;
    let ProxyGet {
        mut url,
        mut token,
        headers,
        build_server,
        passed_back,
        max_hops,
    } = get;
    // Checked here as well as by the caller: the core never sends the token
    // where it may not go, whatever it was handed.
    let mut withheld = false;
    if token.is_some() {
        if let Err(reason) = token_allowed(&url, build_server) {
            log(
                Level::Warn,
                format!("pharos: proxy token withheld reason={reason}"),
            );
            token = None;
            withheld = true;
        }
    }
    // Why a hop carries no token, when it carries none: the request never
    // had one, it was withheld from plain http off the build's server, or
    // an earlier hop dropped it. Stable label strings.
    let started_with_token = token.is_some();
    let no_token_reason = || {
        if started_with_token {
            "token_dropped_earlier"
        } else if withheld {
            "token_withheld_insecure"
        } else {
            "no_token"
        }
    };
    let mut hops = 0usize;
    // Once a redirect leaves the original origin, the caller's headers
    // keep only those that may cross origins, for the rest of the request,
    // as the token does.
    let home = origin_of(&url);
    let mut left_home = false;
    let resp = loop {
        let mut sent = Vec::with_capacity(headers.len() + 1);
        if let Some(token) = &token {
            sent.push(("X-Emby-Token".to_owned(), token.clone()));
        }
        left_home |= origin_of(&url) != home;
        if left_home {
            let dropped: Vec<&str> = headers
                .iter()
                .map(|(n, _)| n.as_str())
                .filter(|n| !crosses_origins(n))
                .collect();
            if !dropped.is_empty() {
                log(
                    Level::Info,
                    format!(
                        "pharos: proxy redirect headers verdict=dropped reason=cross_origin names={}",
                        dropped.join(",")
                    ),
                );
            }
            sent.extend(headers.iter().filter(|(n, _)| crosses_origins(n)).cloned());
        } else {
            sent.extend(headers.iter().cloned());
        }
        let resp = client
            .get(&url, &sent)
            .await
            .map_err(|e| format!("stage=send {e}"))?;
        // Only a real redirect is followed: a 304 (or any other 3xx) is the
        // answer, its ETag passed back, whatever Location it carries.
        if !follows(resp.status()) {
            break resp;
        }
        let Some(next) = resp.redirect() else {
            break resp;
        };
        let next = next.map_err(|e| format!("stage=redirect location not understood: {e}"))?;
        if hops >= max_hops {
            return Err(format!("stage=redirect more than {max_hops} redirects"));
        }
        hops += 1;
        let status = resp.status();
        match redirect_hop(&url, &next, build_server) {
            // The verdict says what the next request carries: a token
            // dropped on an earlier hop stays dropped, even back on its own
            // origin.
            Hop::FollowWithToken if token.is_some() => log(
                Level::Debug,
                format!("pharos: proxy redirect verdict=follow_with_token status={status}"),
            ),
            Hop::FollowWithToken => log(
                Level::Debug,
                format!(
                    "pharos: proxy redirect verdict=follow_without_token reason={} status={status}",
                    no_token_reason()
                ),
            ),
            // Every hop without the token says why, whether this hop
            // dropped it or it was already gone.
            Hop::FollowWithoutToken => {
                let reason = if token.take().is_some() {
                    "cross_origin"
                } else {
                    no_token_reason()
                };
                log(
                    Level::Info,
                    format!(
                        "pharos: proxy redirect verdict=follow_without_token reason={reason} status={status}"
                    ),
                );
            }
            Hop::Refuse(reason) => {
                log(
                    Level::Warn,
                    format!(
                        "pharos: proxy redirect verdict=refused reason={reason} status={status}"
                    ),
                );
                return Err(format!("stage=redirect refused: {reason}"));
            }
        }
        url = next;
    };
    let status = resp.status();
    let back = passed_back
        .iter()
        .filter_map(|name| resp.header(name).map(|v| ((*name).to_owned(), v)))
        .collect();
    let body = resp
        .body()
        .await
        .map_err(|e| format!("stage=body status={status} {e}"))?;
    Ok((status, back, body))
}

/// Whether a response with `status` is a redirect the proxy follows.
fn follows(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// An error and every cause under it, `outer: inner: ...`, for a log line.
/// reqwest's own `Display` names only the top layer ("error sending
/// request"), which hides whether the connection was refused, reset or
/// never resolved.
pub fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// A bounded label for a proxied request's route, for logs: the path's first
/// segment when proxied, `subtitle_image` for image subtitle frames, else
/// `other`. Never the path itself, which carries item ids.
pub fn proxy_segment(path: &str) -> &'static str {
    if path.contains("/subtitles/") && path.contains("/images/") {
        return "subtitle_image";
    }
    let first = path
        .trim_start_matches('/')
        .split(['/', '?'])
        .next()
        .unwrap_or_default();
    PROXIED
        .iter()
        .find(|s| **s == first)
        .copied()
        .unwrap_or("other")
}

/// Every segment the proxy registers a handler for.
pub fn proxied_segments() -> &'static [&'static str] {
    &PROXIED
}

/// An artwork `src` (`/Items/{id}/Images/…`) as the server's own URL, which
/// the WebView loads and caches itself; any other `src` as it is.
pub fn art_src(base: &str, src: &str) -> String {
    if is_server_art(src) {
        server_url(base, src)
    } else {
        src.to_owned()
    }
}

/// Whether `src` is artwork on the server (`/Items/{id}/Images/…`).
pub fn is_server_art(src: &str) -> bool {
    src.starts_with('/') && src.contains("/Images/")
}

/// Where the page loads artwork from in `shell`. The phone's WebView cannot
/// resolve the server's name, though the app's own resolver can, so there
/// artwork stays relative and goes through the page proxy; the TV loads it
/// straight from the server (see `android_page::art_src`).
pub fn art_src_for(shell: Shell, base: &str, src: &str) -> String {
    match shell {
        Shell::AndroidPhone => src.to_owned(),
        Shell::AndroidTv | Shell::Browser => art_src(base, src),
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
    visualiser_start_for_js(id, style, false)
}

/// As [`visualiser_start_js`]; with `host` the page reads its sound from the
/// frames the app pushes ([`visualiser_frame_js`]) instead of an `<audio>`
/// element, because the song plays in the host's engine (ADR-0133). The
/// script is unchanged without it.
pub fn visualiser_start_for_js(id: u64, style: &str, host: bool) -> String {
    let style = serde_json::Value::String(style.to_owned());
    let hosted = if host {
        "window.__pharosVisHost = true;\n"
    } else {
        ""
    };
    format!(
        r#"{hosted}const all = window.__pharosVisRuns = window.__pharosVisRuns || {{}};
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

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard padded base64, for bytes the page decodes with `atob`.
pub fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                let at = ((n >> (18 - 6 * i)) & 63) as usize;
                out.push(char::from(BASE64.get(at).copied().unwrap_or(b'=')));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The script that hands the page the host's frame: `window.__pharosVisFrame`
/// holds the song's rate, the bin bytes and the waveform samples, stamped
/// with the page's own clock when it arrives. The page's visualiser reads it
/// each animation frame and takes a stamp older than `wire::FRESH_MS` as
/// "nothing playing".
pub fn visualiser_frame_js(frame: &super::ports::VisFrame) -> String {
    let bins = base64(&frame.bins);
    let wave: Vec<u8> = frame.wave.iter().map(|s| *s as u8).collect();
    let wave = base64(&wave);
    let rate = frame.sample_rate;
    format!(
        r#"(() => {{
  const d = (s) => {{ const b = atob(s); const a = new Uint8Array(b.length);
    for (let i = 0; i < b.length; i++) a[i] = b.charCodeAt(i); return a; }};
  window.__pharosVisFrame = {{ rate: {rate}, at: performance.now(),
    bins: d("{bins}"), wave: new Int8Array(d("{wave}").buffer) }};
}})();"#
    )
}

/// How often a frame is pushed while the song plays: about 30 Hz.
pub const FRAME_PUSH_MS: u32 = 33;
/// How often the host is asked again while it has no frame.
pub const FRAME_IDLE_MS: u32 = 250;

/// What the frame pump does after asking the host for a frame: the frame to
/// push, if any, and how long to wait before asking again. When frames stop
/// after some were sent, one silent frame goes first, so the picture falls
/// quiet instead of freezing on the last notes; then nothing is sent and the
/// page goes idle with the stamp.
pub fn next_push(
    last_rate: &mut Option<u32>,
    pulled: Option<super::ports::VisFrame>,
) -> (Option<super::ports::VisFrame>, u32) {
    match pulled {
        Some(frame) => {
            *last_rate = Some(frame.sample_rate);
            (Some(frame), FRAME_PUSH_MS)
        }
        None => (
            last_rate.take().map(super::ports::VisFrame::silent),
            FRAME_IDLE_MS,
        ),
    }
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

/// Where the page loads the vendored JASSUB from (ADR-0057); `ui` is a
/// proxied segment, so it comes from the server like the stylesheet.
pub const JASSUB_DIR: &str = "/ui/assets/vendor/jassub";

/// How long the styled renderer may take to get ready before the player
/// keeps the plain cues; the web build's wait.
pub const STYLED_READY_MS: u32 = 45_000;

/// How long a paused redraw waits for an animation frame before a timer
/// stands in: about six frames at 60 Hz, well inside the ready deadline.
const STYLED_TICK_FALLBACK_MS: u32 = 100;

/// The script that starts styled-subtitle run `run` on `<video id=video_id>`:
/// JASSUB drawing the script at `url` with the script's own `fonts` and the
/// subtitle font `pack` (family, file under `/ui/assets/fonts/`). It answers
/// `"ready"` or `"failed: …"`.
///
/// The worker loads from its own path: it finds its wasm and its pthread
/// workers relative to `import.meta.url`, which a `blob:` URL cannot resolve
/// against ("Failed to construct 'URL'", B1216). The fonts reach JASSUB as
/// `blob:` URLs the page fetched itself, kept on `window` for the next
/// track, and the script as `subContent`.
pub fn styled_start_js(
    video_id: &str,
    url: &str,
    fonts: &[String],
    pack: &[(&str, &str)],
    run: u64,
) -> String {
    let vid = serde_json::Value::String(video_id.to_owned());
    let sub = serde_json::Value::String(url.to_owned());
    let fonts = serde_json::to_string(fonts).unwrap_or_else(|_| "[]".to_owned());
    let pack = serde_json::to_string(
        &pack
            .iter()
            .map(|(family, file)| [*family, *file])
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".to_owned());
    let dir = serde_json::Value::String(JASSUB_DIR.to_owned());
    format!(
        r#"const run = {run}, vid = {vid}, sub = {sub}, fonts = {fonts}, pack = {pack}, dir = {dir};
const blob = async (path, type) => {{
  const r = await fetch(path);
  if (!r.ok) throw new Error(path.split("?")[0] + " answered " + r.status);
  return URL.createObjectURL(new Blob([await r.arrayBuffer()], {{ type }}));
}};
const assets = window.__pharosJassubAssets = window.__pharosJassubAssets || {{}};
// The script's own font URLs, revoked if the start fails before a run owns
// them, including any a fetch still finishes after the failure.
const made = [];
let owned = false, failed = false;
const ownBlob = (f) => blob(f, "font/ttf").then((u) => {{ if (failed) URL.revokeObjectURL(u); else made.push(u); return u; }});
// The deadline covers the asset fetches too, so a slow proxy cannot keep
// plain cues up past it.
const deadline = new Promise((_, no) => setTimeout(() => no(new Error("not ready after {ready_ms} ms")), {ready_ms}));
deadline.catch(() => {{}});
const within = (p) => Promise.race([p, deadline]);
try {{
  if (!window.JASSUB) {{
    await within(new Promise((ok, no) => {{
      const s = document.createElement("script");
      s.src = dir + "/jassub.js";
      s.onload = ok;
      s.onerror = () => no(new Error("jassub.js did not load"));
      document.head.append(s);
    }}));
  }}
  const at = (file) => new URL(dir + "/" + file, location.href).href;
  assets.pack = assets.pack || {{}};
  const [subContent, own] = await within(Promise.all([
    (async () => {{ if (!assets.font) assets.font = await blob(dir + "/default.woff2", "font/woff2"); }})(),
    ...pack.filter(([family]) => !assets.pack[family]).map(async ([family, file]) => {{
      assets.pack[family] = await blob("/ui/assets/fonts/" + file, "font/ttf").catch(() => null);
    }}),
  ]).then(() => Promise.all([
    fetch(sub).then((r) => {{ if (!r.ok) throw new Error("the styled script answered " + r.status); return r.text(); }}),
    Promise.all(fonts.map((f) => ownBlob(f).catch(() => null))).then((l) => l.filter(Boolean)),
  ])));
  const runs = window.__pharosStyledRuns = window.__pharosStyledRuns || {{}};
  if (runs[run] === "gone") {{ delete runs[run]; throw new Error("stopped before it started"); }}
  const v = document.getElementById(vid);
  if (!v) throw new Error("no <video id=" + vid + ">");
  const availableFonts = {{ "liberation sans": assets.font }};
  for (const [family] of pack) if (assets.pack[family]) availableFonts[family] = assets.pack[family];
  const j = new JASSUB({{
    video: v, subContent, workerUrl: at("jassub-worker.js"), wasmUrl: at("jassub-worker.wasm"),
    modernWasmUrl: at("jassub-worker-modern.wasm"),
    availableFonts, fonts: own, defaultFont: "liberation sans", queryFonts: false, debug: true,
  }});
  // Keep the counters, drop the console line the debug mode writes per frame.
  if (j.debug) j.debug.onsubtitleFrameCallback = null;
  // A resized canvas needs its lift measured again.
  const ro = new ResizeObserver(() => window.__pharosSend && window.__pharosSend({{ t: "styledresize", run }}));
  if (j._canvas) ro.observe(j._canvas);
  runs[run] = {{ j, own, ro }};
  owned = true;
  window.__pharosJassub = j;
  await within(j.ready);
  if (!runs[run]) return "failed: stopped while starting";
  if (!v.paused) return "ready";
  // A paused video sends no frame callbacks, so draw its frame now, and once
  // more after JASSUB's own observer has resized and cleared the canvas.
  const frame = () => ({{ mediaTime: v.currentTime, width: v.videoWidth, height: v.videoHeight }});
  // A hidden page runs no animation frames, so a timer stands in for one
  // rather than letting the deadline fail a healthy start.
  const tick = () => new Promise((ok) => {{ requestAnimationFrame(() => ok()); setTimeout(ok, {TICK_FALLBACK_MS}); }});
  try {{
    await within(j.manualRender(frame(), true));
    await within(tick()); await within(tick());
    if (runs[run] && v.paused) await within(j.manualRender(frame(), true));
    return "ready paused_redraw=ok";
  }} catch (e) {{
    // A failed draw can leave JASSUB busy for good, so keep the plain cues.
    return "failed: paused redraw " + ((e && e.message) || e);
  }}
}} catch (e) {{
  failed = true;
  if (!owned) made.forEach((u) => URL.revokeObjectURL(u));
  return "failed: " + ((e && e.message) || e);
}}"#,
        ready_ms = STYLED_READY_MS,
        TICK_FALLBACK_MS = STYLED_TICK_FALLBACK_MS,
    )
}

/// The script that stops styled-subtitle run `run`, or marks it `"gone"` so
/// a start still on its way draws nothing.
pub fn styled_stop_js(run: u64) -> String {
    format!(
        r#"const runs = window.__pharosStyledRuns = window.__pharosStyledRuns || {{}};
const r = runs[{run}];
if (r && r.j) {{
  if (r.ro) r.ro.disconnect();
  try {{ r.j.destroy(); }} catch (e) {{}}
  r.own.forEach((u) => URL.revokeObjectURL(u));
  if (window.__pharosJassub === r.j) window.__pharosJassub = null;
  delete runs[{run}];
}} else {{ runs[{run}] = "gone"; }}"#
    )
}

/// The script that measures the styled canvas against the control bar for
/// the lift (B634): `[canvas bottom, player bottom, bar height]` in CSS px,
/// laid-out offsets as the web build reads them, or `null` with no canvas.
pub const STYLED_LIFT_MEASURE_JS: &str = r#"const c = document.querySelector(".pharos-video-frame > canvas.JASSUB");
if (!c) return null;
const f = c.parentElement, p = f.closest(".pharos-player");
const bar = p && p.querySelector(".pharos-controls");
if (!bar) return null;
return [f.getBoundingClientRect().top + c.offsetTop + c.offsetHeight, p.getBoundingClientRect().bottom, bar.offsetHeight];"#;

/// The script that moves the styled canvas up by `lift` px (pharos.css turns
/// `--pharos-ass-lift` into a transform).
pub fn styled_lift_set_js(lift: u32) -> String {
    format!(
        r#"const c = document.querySelector(".pharos-video-frame > canvas.JASSUB");
if (c) c.parentElement.style.setProperty("--pharos-ass-lift", "{lift}");"#
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

/// A D-pad stop in a remote player whose Left and Right scrub, as they do
/// on the picture, and whose OK commits or pauses as the picture's does:
/// the seek bar (B1330).
pub const REMOTE_SLIDERS: &str = "#pharos-seek";

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

/// Controls whose arrows no Dioxus handler takes: a poster tile and a
/// letter of the browse grid's jump rail. Outside a modal dialog, nothing
/// above them in the page has an `onkeydown` either (B1404).
pub const PAGE_ONLY_ARROWS: &str = "a.pharos-poster, button.pharos-letter";

/// An arrow on a [`PAGE_ONLY_ARROWS`] control goes to the bridge straight
/// from the window's capture phase and stops there, so it never reaches the
/// root listener Dioxus keeps for any `onkeydown` in the page. That
/// listener answers every key with a synchronous request to Rust, about
/// 45 ms on the Shield, and a held arrow's repeats queued behind it for
/// 300-500 ms (B1404, V1374). Window listeners in the capture phase,
/// registered before this one, still hear the key.
pub const ARROWS_SKIP_VDOM_JS: &str = r#"
addEventListener("keydown", (e) => {
  const t = e.target;
  if (!e.key.startsWith("Arrow") || !t || !t.matches || !t.matches('__PAGE_ONLY_ARROWS__')) return;
  if (t.closest("[aria-modal='true']")) return;
  bridgeKey(e);
  e.stopPropagation();
}, true);
"#;

/// How long OK is held on a tile that plays on a hold before it plays
/// (B1340, V1311). A pointer's hold waits as long (views.rs).
pub const TILE_HOLD_MS: u32 = 600;

/// OK on a tile that plays on a hold (spec 038 FR-109, B1340, V1311): a
/// press opens its title page, as the WebView's own click would, and a
/// hold of [`TILE_HOLD_MS`] presses the tile's undrawn play link (Alison,
/// 2026-11-11). The WebView's own click is held back on every Enter of the
/// hold, its repeats included, so a hold never opens the page as well.
pub const TILE_HOLD_JS: &str = r#"
let tileHold = null;
const tilePlay = (t) => {
  const tile = t && t.closest && t.closest("a.pharos-poster");
  const li = tile && tile.parentElement;
  const play = li && li.querySelector(":scope > a.pharos-poster-play");
  return play ? { tile, play } : null;
};
// A hold whose tile lost the focus, or whose page was hidden, plays and
// opens nothing; its OK stays held back until the key comes up. A fresh
// (not repeated) press with a hold still set means its keyup never came:
// the old hold is dropped and the press starts anew.
const holdPlay = (at, via) => {
  at.done = true;
  at.play.click();
  console.info("pharos: keys: tile ok verdict=play held_ms=" + Math.round(performance.now() - at.started) + " via=" + via);
};
const dropHold = () => {
  if (!tileHold) return;
  clearTimeout(tileHold.timer);
  tileHold.done = true;
};
addEventListener("focusout", (e) => { if (tileHold && e.target === tileHold.tile) dropHold(); }, true);
addEventListener("blur", dropHold);
document.addEventListener("visibilitychange", dropHold);
addEventListener("keydown", (e) => {
  if (!e.isTrusted || e.key !== "Enter") return;
  if (tileHold && !e.repeat) { dropHold(); tileHold = null; }
  const at = tileHold || tilePlay(e.target);
  if (!at) return;
  e.preventDefault();
  if (tileHold) return;
  // Input times, not handler times: a busy page can run a handler late.
  at.started = e.timeStamp;
  tileHold = at;
  at.timer = setTimeout(() => {
    if (tileHold !== at || at.done) return;
    holdPlay(at, "timer");
  }, __TILE_HOLD_MS__);
}, true);
addEventListener("keyup", (e) => {
  if (e.key !== "Enter" || !tileHold) return;
  const at = tileHold;
  tileHold = null;
  clearTimeout(at.timer);
  if (at.done) return;
  e.preventDefault();
  // A busy page can run the hold's timer late, after the release: an OK
  // held as long as a hold still plays. Measured between the key events'
  // own times, so a late keyup after a short press still opens.
  const held = e.timeStamp - at.started;
  if (held >= __TILE_HOLD_MS__) { holdPlay(at, "release"); return; }
  at.tile.click();
  console.info("pharos: keys: tile ok verdict=open held_ms=" + Math.round(held));
}, true);
"#;

/// The bridge script `template` with its tick intervals, player selectors
/// and Info key filled in.
/// What OK on the picture presses instead of pausing, while one shows:
/// Up Next's Play now, or the skip intro/credits button (FR-604).
pub const OK_PRESSES: &str = ".pharos-up-next-play, .pharos-up-next-pill-play, .pharos-skip";

/// What the remote's Back closes, with an Escape, before it goes back a page
/// (FR-607): dialogs, menus, an open search list, the player's settings
/// sheet and its queue.
pub const OVERLAYS: &str = r#"[aria-modal="true"], [role="dialog"], [role="menu"], [role="listbox"], .pharos-control-menus.is-open, .pharos-watch-queue"#;

pub fn bridge_script(template: &str) -> String {
    template
        .replace("__BAR_KEYS__", BAR_KEY_JS)
        .replace("__TILE_HOLD__", TILE_HOLD_JS)
        .replace("__ARROWS_SKIP_VDOM__", ARROWS_SKIP_VDOM_JS)
        .replace("__GRID_VIEW__", GRID_VIEW_JS)
        .replace("__MEASURE__", MEASURE_JS)
        .replace("__MEASURE_ATTR__", MEASURE_ATTR)
        .replace("__PAGE_ONLY_ARROWS__", PAGE_ONLY_ARROWS)
        .replace("__TILE_HOLD_MS__", &TILE_HOLD_MS.to_string())
        .replace("__VIDEO_TICK_MS__", &VIDEO_TICK_MS.to_string())
        .replace("__AUDIO_TICK_MS__", &AUDIO_TICK_MS.to_string())
        .replace("__REMOTE_PLAYERS__", REMOTE_PLAYERS)
        .replace("__PLAYER_HOME__", PLAYER_HOME)
        .replace("__REMOTE_SKIPS__", REMOTE_SKIPS)
        .replace("__OK_PRESSES__", OK_PRESSES)
        .replace("__OVERLAYS__", OVERLAYS)
        .replace("__GRID__", GRID)
        .replace("__REMOTE_SLIDERS__", REMOTE_SLIDERS)
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
    /// The rendered width of the element `id` that carries [`MEASURE_ATTR`],
    /// as it appears and whenever it resizes.
    Width { id: String, w: f64 },
    /// Where the browse grid on `path` stands once its scroll or focus has
    /// settled: the tile with the focus, else the first in view, and how far
    /// below the viewport's top it sits (FR-603).
    Place { path: String, tile: u32, top: f64 },
    /// Where the browse grid on `path` stands as it scrolls or the
    /// viewport changes, for keeping only the rows near the screen in the
    /// page (B1406).
    Grid {
        path: String,
        #[serde(flatten)]
        view: super::grid_window::GridView,
    },
    /// The remote's Back closed an open dialog or menu (`what`, its class
    /// or role) instead of going back a page (FR-607). With `restore` the
    /// WebView already went back an entry, which Rust puts back.
    #[serde(rename = "backclose")]
    BackClose {
        #[serde(default)]
        what: String,
        #[serde(default)]
        restore: bool,
    },
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
    /// Styled-subtitle run `run`'s canvas changed size, so its lift is
    /// measured again.
    #[serde(rename = "styledresize")]
    StyledResize { run: u64 },
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
    /// The focus is on a field: text, a text area or a slider (FR-606).
    pub field: bool,
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
    /// The styled renderer's own skipped and drawn frames (JASSUB's debug
    /// counters), while one draws over this video.
    #[serde(default)]
    pub sub_skipped: Option<u64>,
    #[serde(default)]
    pub sub_drawn: Option<u64>,
    /// A throttled `timeupdate`, which Rust hears only this way.
    #[serde(default)]
    pub tick: bool,
    /// The event's type (`playing`, `waiting`, ...), which the play's trace
    /// reads (B1367).
    #[serde(default)]
    pub ev: String,
    /// `networkState`.
    #[serde(default)]
    pub ns: u16,
    /// Media buffered ahead of the play head, ms; 0 in a gap.
    #[serde(default)]
    pub fwd: u64,
    /// The buffered ranges, `start-end` in ms, comma separated: only a
    /// `waiting` carries them.
    #[serde(default)]
    pub buffered: String,
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

    /// Where the media had data up to from the play head when this state
    /// arrived, for the seek bar's buffered range (B1386); 0 in a gap, as
    /// the browser's [`buffered_to_ms`](super::platform) answers.
    pub fn buffered_to_ms(&self) -> u64 {
        if self.fwd == 0 {
            return 0;
        }
        secs_to_ms(self.pos).saturating_add(self.fwd)
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
    /// `Hls.Events.FRAG_LOADED` for a main or audio fragment.
    Frag {
        bytes: u64,
        load_ms: u64,
        media_ms: u64,
        /// The fragment's sequence number.
        #[serde(default)]
        sn: u64,
        /// Request start to first byte, ms.
        #[serde(default)]
        ttfb_ms: u64,
        #[serde(default)]
        track: FragTrack,
    },
    /// `Hls.Events.FRAG_BUFFERED`: a main or audio fragment parsed and
    /// appended to the source buffer. Its numbers are the download's, as in
    /// [`HlsEvent::Frag`].
    Buffered {
        #[serde(default)]
        bytes: u64,
        #[serde(default)]
        load_ms: u64,
        #[serde(default)]
        sn: u64,
        #[serde(default)]
        ttfb_ms: u64,
        #[serde(default)]
        track: FragTrack,
    },
}

/// Which rendition a loaded fragment belongs to (hls.js's `frag.type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FragTrack {
    #[default]
    Main,
    Audio,
}

/// The play trace's signal for a media event, `None` for an event the
/// trace does not read (B1367, V1337). A `timeupdate` reaches Rust only as a
/// tick.
pub fn signal_of(ev: &str, tick: bool) -> Option<Signal> {
    match ev {
        "playing" => Some(Signal::Playing),
        "waiting" => Some(Signal::Waiting),
        "seeking" => Some(Signal::Seeking),
        "seeked" => Some(Signal::Seeked),
        "pause" => Some(Signal::Pause),
        "timeupdate" if tick => Some(Signal::TimeUpdate),
        _ => None,
    }
}

/// The `<video>` as the trace reads it at `now_ms`.
pub fn clip_of(media: &Media, now_ms: u64) -> Clip {
    Clip {
        now_ms,
        position_ms: secs_to_ms(media.pos),
        paused: media.paused,
        ready_state: media.ready,
        network_state: media.ns,
        forward_ms: media.fwd,
        buffered: media.buffered.clone(),
    }
}

/// The trace's appended segment for a loaded fragment.
pub fn appended_of(sn: u64, bytes: u64, ttfb_ms: u64, load_ms: u64, track: FragTrack) -> Appended {
    Appended {
        track: match track {
            FragTrack::Main => Track::Video,
            FragTrack::Audio => Track::Audio,
        },
        index: usize::try_from(sn).unwrap_or_default(),
        bytes,
        ttfb_ms,
        load_ms,
    }
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

/// The attribute that has the page report an element's width, so a touch
/// on the seek bars can be turned into a position without a layout call.
pub const MEASURE_ATTR: &str = "data-measure";

/// Script, in the bridge: sends the width of every element with
/// [`MEASURE_ATTR`] as it is added and whenever it resizes.
pub const MEASURE_JS: &str = r#"
const measured = new ResizeObserver((entries) => {
  for (const e of entries) {
    if (e.target.id) send({ t: "width", id: e.target.id, w: e.target.getBoundingClientRect().width });
  }
});
const measure = (n) => {
  if (n.nodeType !== 1) return;
  if (n.matches("[__MEASURE_ATTR__]")) measured.observe(n);
  for (const m of n.querySelectorAll("[__MEASURE_ATTR__]")) measured.observe(m);
};
new MutationObserver((records) => {
  for (const r of records) for (const n of r.addedNodes) measure(n);
}).observe(document.documentElement, { childList: true, subtree: true });
measure(document.documentElement);
"#;

/// The browse grid whose children are the tiles, as `web` finds them.
const GRID: &str = ".pharos-browse .pharos-grid";

/// Script, in the bridge: the browse grid's geometry, sent as the page
/// scrolls (at most every 80 ms) or resizes, and on Rust's asking
/// (`window.__pharosGridView`), so Rust keeps only the rows near the screen
/// in the page (B1406). Each tile carries its place in the grid as
/// `data-at`; a row's pitch is measured from one tile to the one a row
/// below, and `seq` is the last placement carried out
/// ([`scroll_to_tile_js`]).
pub const GRID_VIEW_JS: &str = r#"
let viewTimer = 0, viewLast = 0;
const gridView = () => {
  const grid = document.querySelector('__GRID__');
  const first = grid && grid.querySelector(":scope > [data-at]");
  if (!first) return;
  const css = getComputedStyle(grid);
  const cols = css.gridTemplateColumns.split(" ").filter(Boolean).length;
  const gap = parseFloat(css.rowGap) || 0;
  const r = first.getBoundingClientRect();
  const below = grid.querySelector(`:scope > [data-at="${+first.dataset.at + cols}"]`);
  const pitch = below ? below.getBoundingClientRect().top - r.top : r.height + gap;
  if (!(cols > 0 && pitch > 0)) return;
  send({ t: "grid", path: location.pathname + location.search,
    top: grid.getBoundingClientRect().top, height: innerHeight, cols, pitch, gap,
    seq: window.__pharosGridSeq || 0 });
};
const gridViewSoon = () => {
  if (viewTimer) return;
  viewTimer = setTimeout(() => {
    viewTimer = 0;
    viewLast = performance.now();
    gridView();
  }, Math.max(0, viewLast + 80 - performance.now()));
};
addEventListener("scroll", gridViewSoon, { passive: true });
addEventListener("resize", gridViewSoon);
window.__pharosGridView = gridView;
"#;

/// Script: send where the browse grid stands now ([`GRID_VIEW_JS`]).
pub const MEASURE_GRID_JS: &str = "if (window.__pharosGridView) window.__pharosGridView();";

/// Script: scroll the browse tile at `at` to the top of the page and focus
/// its link, so the remote's next press starts from the section jumped to
/// (B1028). Answers whether the tile was in the page. `seq` numbers the
/// placement, which the grid's geometry carries from then on (B1406).
pub fn scroll_to_tile_js(at: u32, seq: u64) -> String {
    format!(
        r#"
window.__pharosGridSeq = {seq};
const grid = document.querySelector({grid});
const tile = grid ? grid.querySelector(':scope > [data-at="{at}"]') : null;
if (!tile) return false;
tile.scrollIntoView(true);
const link = tile.querySelector("a[href], button, [tabindex]") || tile;
if (link.focus) link.focus({{ preventScroll: true }});
return true;
"#,
        grid = serde_json::Value::String(GRID.to_owned()),
    )
}

/// Script: put the browse tile at `at` back `top` CSS px below the top of
/// the viewport, where it was when the grid was left, and focus it (FR-603).
pub fn scroll_tile_to_js(at: u32, top: f64, seq: u64) -> String {
    let top = if top.is_finite() { top } else { 0.0 };
    format!(
        r#"
window.__pharosGridSeq = {seq};
const grid = document.querySelector({grid});
const tile = grid ? grid.querySelector(':scope > [data-at="{at}"]') : null;
if (!tile) return false;
window.scrollBy(0, tile.getBoundingClientRect().top - ({top}));
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

    /// CR 4642574391: `previous` holds the original and each redirect
    /// before this one, so the tenth redirect sees ten and is followed;
    /// only an eleventh is refused.
    #[test]
    fn the_tenth_download_redirect_is_followed() {
        assert!(!redirects_exceeded(1), "the first redirect");
        assert!(!redirects_exceeded(10), "the tenth redirect");
        assert!(redirects_exceeded(11), "an eleventh redirect");
    }

    /// The device token goes only over https, or to the build's own server
    /// at its own origin; any other plain-http URL is refused with the
    /// origin named.
    #[test]
    fn the_device_token_goes_only_over_https_or_to_the_builds_server() {
        let dev = "http://192.168.1.5:8096";
        let https = "https://media.example.org";
        assert!(token_allowed("https://media.example.org/Items", https).is_ok());
        assert!(token_allowed("https://other.example.org/Items", dev).is_ok());
        assert!(token_allowed("http://192.168.1.5:8096/Items", dev).is_ok());
        let Err(why) = token_allowed("http://192.168.1.6:8096/Items", dev) else {
            panic!("plain http to another LAN host was allowed");
        };
        assert!(why.contains("192.168.1.6:8096"), "{why}");
        assert!(token_allowed("http://192.168.1.5:9000/Items", dev).is_err());
        assert!(token_allowed("http://media.example.org/Items", https).is_err());
        assert!(token_allowed("ftp://media.example.org/Items", https).is_err());
    }

    /// Origins compare as the HTTP client reaches them. A backslash ends
    /// the authority for http(s) there, so a URL that reads as the build's
    /// server after an `@` goes to another host, and gets no token; an IPv4
    /// shorthand or a percent-encoded host is the same origin it names.
    #[test]
    fn origins_compare_as_the_http_client_parses_them() {
        let lan = "http://media.example.org:8096";
        assert!(
            token_allowed(
                "http://other.example.net\\@media.example.org:8096/Items",
                lan
            )
            .is_err(),
            "a request reqwest sends to another host was given the token"
        );
        assert!(token_allowed("http://127.1:8096/Items", "http://127.0.0.1:8096").is_ok());
        assert!(token_allowed("http://medi%61.example.org:8096/Items", lan).is_ok());
        assert_eq!(
            redirect_hop(
                "https://media.example.org/a",
                "https://other.example.net\\@media.example.org/b",
                lan
            ),
            Hop::FollowWithoutToken
        );
    }

    /// A Unicode host is the punycode origin the client sends to: a redirect
    /// between the two spellings keeps the token, and one to another IDN
    /// host goes without it.
    #[test]
    fn a_unicode_host_compares_as_its_punycode_origin() {
        let lan = "http://xn--mdia-bpa.example.org:8096";
        assert_eq!(
            redirect_hop(
                "https://m\u{e9}dia.example.org/a",
                "https://xn--mdia-bpa.example.org/b",
                lan
            ),
            Hop::FollowWithToken
        );
        assert_eq!(
            redirect_hop(
                "https://xn--mdia-bpa.example.org/a",
                "https://m\u{e8}dia.example.org/b",
                lan
            ),
            Hop::FollowWithoutToken
        );
        assert!(token_allowed("http://m\u{e9}dia.example.org:8096/Items", lan).is_ok());
        assert!(token_allowed("http://m\u{e8}dia.example.org:8096/Items", lan).is_err());
    }

    /// A proxied request's redirect: the same origin keeps the token,
    /// another https origin is followed without it, a downgrade to http is
    /// refused, and the build's own plain-http server keeps it on its own
    /// origin.
    #[test]
    fn a_proxy_redirect_keeps_the_token_only_on_its_own_origin() {
        let https = "https://media.example.org";
        let from = "https://media.example.org/Items/1/Images/Primary";
        assert_eq!(
            redirect_hop(from, "https://media.example.org/cache/1.jpg", https),
            Hop::FollowWithToken
        );
        assert_eq!(
            redirect_hop(from, "https://art.example.net/1.jpg", https),
            Hop::FollowWithoutToken
        );
        let Hop::Refuse(why) = redirect_hop(from, "http://media.example.org/1.jpg", https) else {
            panic!("a downgrade to http was followed");
        };
        assert!(why.contains("downgrade"), "{why}");
        let dev = "http://192.168.1.5:8096";
        assert_eq!(
            redirect_hop(
                "http://192.168.1.5:8096/a",
                "http://192.168.1.5:8096/b",
                dev
            ),
            Hop::FollowWithToken
        );
        assert_eq!(
            redirect_hop(
                "http://192.168.1.5:8096/a",
                "http://192.168.1.6:8096/b",
                dev
            ),
            Hop::FollowWithoutToken
        );
        assert!(matches!(
            redirect_hop(from, "ftp://media.example.org/1.jpg", https),
            Hop::Refuse(_)
        ));
    }

    /// A client that cannot be built is an error naming why, and what would
    /// have been sent with it is not sent. (reqwest has no way to force a
    /// build failure, so the builder is a fake.)
    #[test]
    fn a_download_client_that_cannot_be_built_refuses_the_download() {
        let mut sent = false;
        let client = built_client(|| Err::<(), _>("tls backend unavailable"));
        let outcome = client.as_ref().map(|c| {
            sent = true;
            *c
        });
        assert!(!sent, "a request went out without its client");
        let Err(why) = outcome else {
            panic!("a client that was never built was used");
        };
        assert!(why.contains("tls backend unavailable"), "{why}");
        assert!(why.contains("download client"), "{why}");
        assert_eq!(built_client(|| Ok::<_, String>(7)), Ok(7));
    }

    /// A transport that counts the requests it is asked to send and
    /// answers each with a 200 `ok`: nothing goes over a socket.
    struct Counting(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    struct Answer(u16, Vec<u8>);

    impl ProxyResponse for Answer {
        fn status(&self) -> u16 {
            self.0
        }
        fn header(&self, _: &str) -> Option<String> {
            None
        }
        fn redirect(&self) -> Option<Result<String, String>> {
            None
        }
        async fn body(self) -> Result<Vec<u8>, String> {
            Ok(self.1)
        }
    }

    impl ProxyTransport for Counting {
        type Response = Answer;
        async fn get(&self, _: &str, _: &[(String, String)]) -> Result<Answer, String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Answer(200, b"ok".to_vec()))
        }
    }

    /// The proxy path itself with a client that cannot be built: the
    /// request is refused with the reason, and nothing reaches the server.
    /// With a client, the same request does reach it.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_proxy_client_that_cannot_be_built_sends_nothing() {
        let base = "http://media.example.org";
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let logged = std::sync::Mutex::new(Vec::new());
        let log = |_: Level, line: String| logged.lock().unwrap().push(line);
        let get = |url: String| ProxyGet {
            url,
            token: Some("tok".to_owned()),
            headers: &[],
            build_server: "",
            passed_back: &[],
            max_hops: 5,
        };
        let broken = proxy_client::<Counting>(|| Err("tls config: no roots".to_owned()));
        let refused = rt.block_on(proxy_get(&broken, get(format!("{base}/Items")), &log));
        let Err(why) = refused else {
            panic!("a client that was never built sent the request");
        };
        assert!(why.starts_with("stage=client "), "{why}");
        assert!(
            why.contains("proxy client") && why.contains("no roots"),
            "{why}"
        );
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a request was sent"
        );
        assert!(
            logged
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.starts_with("pharos: proxy request refused reason=")
                    && l.contains("no roots")),
            "{:?}",
            logged.lock().unwrap()
        );

        // The control: a built client reaches the same server.
        let built = proxy_client(|| Ok(Counting(hits.clone())));
        let (status, _, body) = rt
            .block_on(proxy_get(&built, get(format!("{base}/Items")), &log))
            .unwrap();
        assert_eq!((status, body.as_slice()), (200, b"ok".as_slice()));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// A transport that answers every request with 200 and keeps the
    /// headers each one was sent with.
    #[derive(Default)]
    struct Recording(std::sync::Mutex<Vec<Vec<(String, String)>>>);

    #[allow(clippy::unwrap_used)]
    impl ProxyTransport for Recording {
        type Response = Answer;
        async fn get(&self, _: &str, headers: &[(String, String)]) -> Result<Answer, String> {
            self.0.lock().unwrap().push(headers.to_vec());
            Ok(Answer(200, Vec::new()))
        }
    }

    /// A transport that answers each request with the next redirect of its
    /// script (200 once the script is done), keeping each request's URL and
    /// whether it carried the token.
    struct Scripted {
        redirects: std::sync::Mutex<std::collections::VecDeque<String>>,
        sent: std::sync::Mutex<Vec<(String, bool)>>,
        /// The header names each request carried.
        names: std::sync::Mutex<Vec<Vec<String>>>,
    }

    impl Scripted {
        fn new(redirects: &[&str]) -> Self {
            Self {
                redirects: std::sync::Mutex::new(
                    redirects.iter().map(|r| (*r).to_owned()).collect(),
                ),
                sent: std::sync::Mutex::new(Vec::new()),
                names: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    struct ScriptedAnswer(Option<String>);

    impl ProxyResponse for ScriptedAnswer {
        fn status(&self) -> u16 {
            if self.0.is_some() {
                302
            } else {
                200
            }
        }
        fn header(&self, _: &str) -> Option<String> {
            None
        }
        fn redirect(&self) -> Option<Result<String, String>> {
            self.0.clone().map(Ok)
        }
        async fn body(self) -> Result<Vec<u8>, String> {
            Ok(b"ok".to_vec())
        }
    }

    #[allow(clippy::unwrap_used)]
    impl ProxyTransport for Scripted {
        type Response = ScriptedAnswer;
        async fn get(
            &self,
            url: &str,
            headers: &[(String, String)],
        ) -> Result<ScriptedAnswer, String> {
            let carries = headers.iter().any(|(n, _)| n == "X-Emby-Token");
            self.sent.lock().unwrap().push((url.to_owned(), carries));
            self.names
                .lock()
                .unwrap()
                .push(headers.iter().map(|(n, _)| n.clone()).collect());
            Ok(ScriptedAnswer(self.redirects.lock().unwrap().pop_front()))
        }
    }

    /// A 304 that carries a Location (as some servers send) is returned with
    /// its ETag after one request: nothing is followed.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_not_modified_answer_is_returned_without_following_its_location() {
        struct NotModified(std::sync::Mutex<usize>);
        struct Answer;
        impl ProxyResponse for Answer {
            fn status(&self) -> u16 {
                304
            }
            fn header(&self, name: &str) -> Option<String> {
                (name == "ETag").then(|| "\"v1\"".to_owned())
            }
            fn redirect(&self) -> Option<Result<String, String>> {
                Some(Ok("https://a.example.org/elsewhere".to_owned()))
            }
            async fn body(self) -> Result<Vec<u8>, String> {
                Ok(Vec::new())
            }
        }
        impl ProxyTransport for NotModified {
            type Response = Answer;
            async fn get(&self, _: &str, _: &[(String, String)]) -> Result<Answer, String> {
                *self.0.lock().unwrap() += 1;
                Ok(Answer)
            }
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let client = proxy_client(|| Ok(NotModified(std::sync::Mutex::new(0))));
        let (status, back, _) = rt
            .block_on(proxy_get(
                &client,
                ProxyGet {
                    url: "https://a.example.org/1".to_owned(),
                    token: Some("tok".to_owned()),
                    headers: &[],
                    build_server: "https://a.example.org",
                    passed_back: &["ETag"],
                    max_hops: 5,
                },
                &|_, _| {},
            ))
            .unwrap();
        assert_eq!(status, 304);
        assert_eq!(back, [("ETag".to_owned(), "\"v1\"".to_owned())]);
        assert_eq!(
            *client.as_ref().unwrap().0.lock().unwrap(),
            1,
            "the 304's Location was followed"
        );
    }

    /// Which requests of a proxied GET over `redirects` carried the token,
    /// starting at `from` with it.
    #[allow(clippy::unwrap_used)]
    fn token_per_hop(from: &str, redirects: &[&str]) -> Vec<(String, bool)> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let client = proxy_client(|| Ok(Scripted::new(redirects)));
        let (status, _, body) = rt
            .block_on(proxy_get(
                &client,
                ProxyGet {
                    url: from.to_owned(),
                    token: Some("tok".to_owned()),
                    headers: &[],
                    build_server: "https://a.example.org",
                    passed_back: &[],
                    max_hops: 5,
                },
                &|_, _| {},
            ))
            .unwrap();
        assert_eq!((status, body.as_slice()), (200, b"ok".as_slice()));
        let sent = client.as_ref().unwrap().sent.lock().unwrap().clone();
        sent
    }

    /// Once a redirect leaves the token's origin the token is gone for the
    /// rest of the request: A to B to B and back to A sends it on the first
    /// request only, never again, even back on A.
    #[test]
    fn a_token_dropped_on_one_hop_stays_dropped_for_the_rest() {
        let sent = token_per_hop(
            "https://a.example.org/1",
            &[
                "https://b.example.net/2",
                "https://b.example.net/3",
                "https://a.example.org/4",
            ],
        );
        let carried: Vec<bool> = sent.iter().map(|(_, c)| *c).collect();
        assert_eq!(
            carried,
            [true, false, false, false],
            "the token went past its origin: {sent:?}"
        );
        assert_eq!(sent[3].0, "https://a.example.org/4");
    }

    /// CR 4644634678: a caller's credential headers stay on the original
    /// origin. A redirect to another origin keeps only the headers that may
    /// cross ([`CROSS_ORIGIN_HEADERS`]) for the rest of the request; a
    /// same-origin hop keeps them all.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_callers_credential_headers_do_not_follow_a_redirect_off_origin() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let client = proxy_client(|| {
            Ok(Scripted::new(&[
                "https://a.example.org/2",
                "https://b.example.net/3",
                "https://a.example.org/4",
            ]))
        });
        let headers = [
            ("Authorization".to_owned(), "Bearer x".to_owned()),
            ("Cookie".to_owned(), "s=1".to_owned()),
            ("accept".to_owned(), "image/*".to_owned()),
        ];
        let logged = std::sync::Mutex::new(Vec::new());
        rt.block_on(proxy_get(
            &client,
            ProxyGet {
                url: "https://a.example.org/1".to_owned(),
                token: None,
                headers: &headers,
                build_server: "https://a.example.org",
                passed_back: &[],
                max_hops: 5,
            },
            &|_, line| logged.lock().unwrap().push(line),
        ))
        .unwrap();
        let names = client.as_ref().unwrap().names.lock().unwrap().clone();
        let full = ["Authorization", "Cookie", "accept"];
        assert_eq!(names[0], full, "{names:?}");
        assert_eq!(names[1], full, "a same-origin hop lost a header: {names:?}");
        assert_eq!(
            names[2],
            ["accept"],
            "credentials crossed origins: {names:?}"
        );
        assert_eq!(names[3], ["accept"], "credentials came back: {names:?}");
        assert!(logged
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("verdict=dropped reason=cross_origin names=Authorization,Cookie")));
    }

    /// CR 4647753659: a same-origin redirect on a plain-http origin that is
    /// not the build's server sends no token (it was withheld from the
    /// first request) and still logs its verdict, with that reason.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_hop_on_plain_http_off_the_build_server_logs_the_withheld_token() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let client = proxy_client(|| Ok(Scripted::new(&["http://c.example.com/2"])));
        let logged = std::sync::Mutex::new(Vec::new());
        rt.block_on(proxy_get(
            &client,
            ProxyGet {
                url: "http://c.example.com/1".to_owned(),
                token: Some("t".to_owned()),
                headers: &[],
                build_server: "https://a.example.org",
                passed_back: &[],
                max_hops: 5,
            },
            &|_, line| logged.lock().unwrap().push(line),
        ))
        .unwrap();
        let names = client.as_ref().unwrap().names.lock().unwrap().clone();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(
            names.iter().flatten().all(|n| n != "X-Emby-Token"),
            "a token was sent: {names:?}"
        );
        let lines = logged.lock().unwrap().clone();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("verdict=follow_without_token reason=token_withheld_insecure")),
            "{lines:?}"
        );
    }

    /// CR 4647378697: a request that started with no token logs a
    /// same-origin hop as carrying none for that reason, not as a token an
    /// earlier hop dropped, and sends no token on either request.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_hop_of_a_request_with_no_token_says_none_was_sent() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let client = proxy_client(|| Ok(Scripted::new(&["https://a.example.org/2"])));
        let logged = std::sync::Mutex::new(Vec::new());
        rt.block_on(proxy_get(
            &client,
            ProxyGet {
                url: "https://a.example.org/1".to_owned(),
                token: None,
                headers: &[],
                build_server: "https://a.example.org",
                passed_back: &[],
                max_hops: 5,
            },
            &|_, line| logged.lock().unwrap().push(line),
        ))
        .unwrap();
        let names = client.as_ref().unwrap().names.lock().unwrap().clone();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(
            names.iter().flatten().all(|n| n != "X-Emby-Token"),
            "a token was sent: {names:?}"
        );
        let lines = logged.lock().unwrap().clone();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("verdict=follow_without_token reason=no_token")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("token_dropped_earlier")),
            "{lines:?}"
        );
    }

    /// CR 4646280279: a same-origin hop on the token's origin, after the
    /// token was dropped off it, logs what the next request carries: no
    /// token.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_hop_back_after_the_token_was_dropped_logs_no_token() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let client = proxy_client(|| {
            Ok(Scripted::new(&[
                "https://b.example.net/2",
                "https://a.example.org/3",
                "https://a.example.org/4",
            ]))
        });
        let logged = std::sync::Mutex::new(Vec::new());
        rt.block_on(proxy_get(
            &client,
            ProxyGet {
                url: "https://a.example.org/1".to_owned(),
                token: Some("t".to_owned()),
                headers: &[],
                build_server: "https://a.example.org",
                passed_back: &[],
                max_hops: 5,
            },
            &|_, line| logged.lock().unwrap().push(line),
        ))
        .unwrap();
        let lines = logged.lock().unwrap().clone();
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("verdict=follow_with_token")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("verdict=follow_without_token reason=token_dropped_earlier")),
            "{lines:?}"
        );
    }

    /// The control: redirects that stay on the token's origin keep it on
    /// every request.
    #[test]
    fn a_token_is_kept_on_redirects_within_its_origin() {
        let sent = token_per_hop(
            "https://a.example.org/1",
            &["https://a.example.org/2", "https://a.example.org/3"],
        );
        let carried: Vec<bool> = sent.iter().map(|(_, c)| *c).collect();
        assert_eq!(carried, [true, true, true], "{sent:?}");
    }

    /// The proxy core withholds the token itself from a URL it may not go
    /// to (plain http to a server other than the build's), whatever its
    /// caller passed in, and logs why.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_proxy_request_to_a_disallowed_url_sends_no_token() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let logged = std::sync::Mutex::new(Vec::new());
        let log = |_: Level, line: String| logged.lock().unwrap().push(line);
        let client = proxy_client(|| Ok(Recording::default()));
        let get = |url: &str| ProxyGet {
            url: url.to_owned(),
            token: Some("tok".to_owned()),
            headers: &[],
            build_server: "https://media.example.org",
            passed_back: &[],
            max_hops: 5,
        };
        rt.block_on(proxy_get(
            &client,
            get("http://192.168.1.9:8096/Items"),
            &log,
        ))
        .unwrap();
        rt.block_on(proxy_get(
            &client,
            get("https://media.example.org/Items"),
            &log,
        ))
        .unwrap();
        let sent = client.as_ref().unwrap().0.lock().unwrap().clone();
        let carries = |h: &Vec<(String, String)>| h.iter().any(|(n, _)| n == "X-Emby-Token");
        assert!(
            !carries(&sent[0]),
            "the token went over plain http: {sent:?}"
        );
        assert!(carries(&sent[1]), "the token was withheld over https");
        assert!(
            logged
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.starts_with("pharos: proxy token withheld reason=")),
            "{:?}",
            logged.lock().unwrap()
        );
    }

    /// An IPv6 server spelled two ways is one origin: a relative redirect
    /// comes back in the compressed spelling and keeps the token, the
    /// download follows it, and the build's own server matches either way.
    /// A different address is still another origin.
    #[test]
    fn an_ipv6_origin_spelled_two_ways_is_one_origin() {
        let expanded = "https://[2001:0DB8:0:0:0:0:0:1]:8096/Items";
        let compressed = "https://[2001:db8::1]:8096/Videos/1/stream";
        assert_eq!(redirect_hop(expanded, compressed, ""), Hop::FollowWithToken);
        assert_eq!(redirect_verdict(expanded, compressed), Ok(()));
        assert_eq!(
            token_allowed(
                "http://[2001:db8::1]:8096/Items",
                "http://[2001:0db8::0:1]:8096"
            ),
            Ok(())
        );
        let other = "https://[2001:db8::2]:8096/Videos/1/stream";
        assert_eq!(redirect_hop(expanded, other, ""), Hop::FollowWithoutToken);
        assert!(redirect_verdict(expanded, other).is_err());
        assert!(token_allowed(
            "http://[2001:db8::2]:8096/Items",
            "http://[2001:db8::1]:8096"
        )
        .is_err());
    }

    /// The proxy's client that cannot be built is an error naming the proxy
    /// and why, so every proxied request is refused, never sent through a
    /// default client without the TLS config and redirect policy.
    #[test]
    fn a_proxy_client_that_cannot_be_built_refuses_every_request() {
        let mut sent = false;
        let client = built_client_for("proxy", || Err::<(), _>("tls config: no roots"));
        let outcome = client.as_ref().map(|c| {
            sent = true;
            *c
        });
        assert!(!sent, "a request went out without its client");
        let Err(why) = outcome else {
            panic!("a client that was never built was used");
        };
        assert!(why.contains("proxy client"), "{why}");
        assert!(why.contains("no roots"), "{why}");
    }

    /// A download sends the device token, so it follows a redirect only to
    /// the same scheme, host and port. A plain-http server on the LAN keeps
    /// working: http to the same http origin is followed.
    #[test]
    fn a_download_follows_only_same_origin_redirects() {
        let from = "https://pharos.example/Videos/1/master.m3u8?x=1";
        assert_eq!(
            redirect_verdict(from, "https://pharos.example/other"),
            Ok(())
        );
        assert_eq!(
            redirect_verdict(from, "https://PHAROS.example:443/other"),
            Ok(()),
            "case and the default port are the same origin"
        );
        let lan = "http://192.168.1.5:8096/Videos/1/a.ts";
        assert_eq!(
            redirect_verdict(lan, "http://192.168.1.5:8096/b.ts"),
            Ok(())
        );
        for (next, why) in [
            ("https://cdn.example/a", "another host"),
            ("http://pharos.example/a", "https to http"),
            ("https://pharos.example:8443/a", "another port"),
            ("https://user@evil.example/a", "another host"),
            ("ftp://pharos.example/a", "not an http"),
            ("/relative", "not an http"),
        ] {
            let verdict = redirect_verdict(from, next);
            assert!(
                verdict.as_ref().is_err_and(|e| e.contains(why)),
                "{next}: {verdict:?}"
            );
        }
        assert!(redirect_verdict(lan, "http://192.168.1.6:8096/a").is_err());
        assert!(redirect_verdict(lan, "http://192.168.1.5:9000/a").is_err());
        assert!(redirect_verdict(lan, "https://192.168.1.5:8096/a").is_err());
    }

    /// B1323 / V1294: the TV's activity handles every configuration change
    /// a TV makes while the app runs; one it does not list recreates the
    /// NativeActivity, which comes back to a dark window with no page.
    #[test]
    fn the_tv_manifest_keeps_the_activity_through_config_changes() {
        let manifest = include_str!("../../android/AndroidManifest.xml");
        let changes = manifest
            .split("android:configChanges=\"")
            .nth(1)
            .and_then(|r| r.split('"').next())
            .unwrap_or_default();
        let listed: Vec<&str> = changes.split('|').collect();
        for change in [
            "density",
            "keyboard",
            "keyboardHidden",
            "navigation",
            "orientation",
            "screenLayout",
            "screenSize",
            "smallestScreenSize",
            "uiMode",
        ] {
            assert!(listed.contains(&change), "{change} missing from {changes}");
        }
    }

    /// The value of `attr="…"` in the first element of `xml` that has it.
    fn attr<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
        let rest = xml.split(&format!(" {name}=\"")).nth(1)?;
        rest.split('"').next()
    }

    /// B1329 / V1302: the TV app's launcher label, icon, banner and dark
    /// launch theme come from the manifest and `android/res`, which
    /// `scripts/android-apk.sh` lays over dx's project. A TV build lost all
    /// four once, so each piece the APK checker looks for is pinned here.
    #[test]
    fn the_tv_manifest_names_the_pharos_label_icon_banner_and_theme() {
        let manifest = include_str!("../../android/AndroidManifest.xml");
        let app = manifest
            .split("<application")
            .nth(1)
            .and_then(|r| r.split('>').next())
            .unwrap_or_default();
        assert_eq!(attr(app, "android:label"), Some("Pharos"), "{app}");
        assert_eq!(attr(app, "android:icon"), Some("@mipmap/ic_launcher"));
        assert!(attr(app, "android:banner").is_some(), "no banner: {app}");
        assert_eq!(attr(app, "android:theme"), Some("@style/AppTheme"));
        assert!(
            manifest.contains("android.intent.category.LEANBACK_LAUNCHER"),
            "the TV launcher lists only leanback apps"
        );
    }

    /// B1329 / V1302: the overlay's theme keeps the launch window Pharos
    /// Night, the page's own background, so a TV start shows no white.
    #[test]
    fn the_tv_launch_theme_is_pharos_night() {
        let styles = include_str!("../../android/res/values/styles.xml");
        let theme = styles
            .split("<style name=\"AppTheme\"")
            .nth(1)
            .and_then(|r| r.split("</style>").next())
            .unwrap_or_default();
        assert!(
            theme.contains(r#"<item name="android:windowBackground">@color/pharos_night</item>"#),
            "AppTheme's window background is not Pharos Night: {theme}"
        );
        let night = styles
            .split("<color name=\"pharos_night\">")
            .nth(1)
            .and_then(|r| r.split('<').next())
            .unwrap_or_default();
        let css = include_str!("../../assets/pharos.css");
        assert!(
            css.contains(&format!("--px-color-bg: {};", night.to_ascii_lowercase())),
            "pharos_night {night} is not the page background"
        );
    }

    /// B1329 / V1302: the overlay carries its own icon and banner, and the
    /// build script lays it over dx's project, points the banner at it and
    /// refuses an APK the checker fails.
    #[test]
    fn the_tv_overlay_has_its_icon_and_banner_and_the_build_applies_it() {
        let banner = include_bytes!("../../android/res/drawable-xhdpi/pharos_banner.png");
        assert_eq!(&banner[..8], b"\x89PNG\r\n\x1a\n");
        let size = |at: usize| {
            u32::from_be_bytes([banner[at], banner[at + 1], banner[at + 2], banner[at + 3]])
        };
        assert_eq!((size(16), size(20)), (320, 180), "a TV banner is 320x180");
        for icon in [
            include_bytes!("../../android/res/mipmap-mdpi/ic_launcher.webp").as_slice(),
            include_bytes!("../../android/res/mipmap-hdpi/ic_launcher.webp"),
            include_bytes!("../../android/res/mipmap-xhdpi/ic_launcher.webp"),
            include_bytes!("../../android/res/mipmap-xxhdpi/ic_launcher.webp"),
            include_bytes!("../../android/res/mipmap-xxxhdpi/ic_launcher.webp"),
        ] {
            assert_eq!((&icon[..4], &icon[8..12]), (&b"RIFF"[..], &b"WEBP"[..]));
        }
        let foreground = include_str!("../../android/res/drawable-v24/ic_launcher_foreground.xml");
        assert!(foreground.contains("android:pathData="), "{foreground}");

        let script = include_str!("../../../../scripts/android-apk.sh");
        for step in [
            "cp -r crates/pharos-ui/android/res/. \"$main_dir/res/\"",
            "android:banner=\"@drawable/pharos_banner\"",
            "scripts/android-apk-check.sh\" \"$built\"",
        ] {
            assert!(script.contains(step), "android-apk.sh lost: {step}");
        }
    }

    #[test]
    fn a_paths_search_is_its_query() {
        assert_eq!(
            search_of("/ui/browse/s7?genre=animation"),
            "?genre=animation"
        );
        assert_eq!(search_of("/ui/browse/s7"), "");
        assert_eq!(search_of("/ui/title/s7?a=1#x"), "?a=1");
    }

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
                view.contains(&format!("class: \"{name}\""))
                    || view.contains(&format!("class: \"{name} ")),
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
    fn a_kept_place_puts_the_tile_back_where_it_was_and_focuses_it() {
        let js = scroll_tile_to_js(40, 212.5, 3);
        assert!(js.contains(r#"[data-at="40"]"#), "{js}");
        assert!(js.contains("window.__pharosGridSeq = 3;"), "{js}");
        assert!(js.contains("getBoundingClientRect().top - (212.5)"), "{js}");
        assert!(js.contains("link.focus({ preventScroll: true })"), "{js}");
        assert!(scroll_tile_to_js(1, f64::NAN, 0).contains("- (0)"));
        assert_eq!(bridge_script("__GRID__"), GRID);
    }

    #[test]
    fn the_styled_start_loads_the_worker_from_its_own_path() {
        let js = styled_start_js(
            "v",
            "/videos/1/subtitles/2/stream.ass?tier=tv",
            &["/videos/1/attachments/a.ttf".to_owned()],
            &[("bangers", "Bangers-Regular.ttf")],
            7,
        );
        assert!(js.starts_with("const run = 7, vid = \"v\""), "{js}");
        assert!(js.contains(r#"sub = "/videos/1/subtitles/2/stream.ass?tier=tv""#));
        assert!(js.contains(r#"fonts = ["/videos/1/attachments/a.ttf"]"#));
        assert!(js.contains(r#"pack = [["bangers","Bangers-Regular.ttf"]]"#));
        // The worker resolves its wasm against its own URL, so it is never a
        // blob (B1216); the script itself goes in as content.
        assert!(js.contains(r#"workerUrl: at("jassub-worker.js")"#), "{js}");
        assert!(!js.contains("assets.worker"), "{js}");
        assert!(js.contains("subContent"));
        assert!(!js.contains("subUrl"), "{js}");
        assert!(js.contains("debug: true"));
        assert!(js.contains(&format!("not ready after {STYLED_READY_MS} ms")));
    }

    #[test]
    fn the_styled_start_bounds_its_fetches_quiets_its_log_and_redraws_when_paused() {
        let js = styled_start_js("v", "/s.ass", &[], &[("bangers", "B.ttf")], 3);
        let at = |needle: &str| {
            js.find(needle)
                .unwrap_or_else(|| panic!("{needle} in {js}"))
        };
        // The deadline starts before the first fetch and covers each of them.
        assert!(at("const deadline") < at("blob(dir"), "{js}");
        assert!(js.contains("await within(j.ready)"), "{js}");
        assert!(js.contains(".filter(([family]) => !assets.pack[family]).map(async"));
        assert!(js.contains("j.debug.onsubtitleFrameCallback = null"));
        assert!(js.contains(r#"t: "styledresize", run"#));
        assert!(js.contains("runs[run] = { j, own, ro }"));
        // Every redraw and tick of the paused draw races the deadline.
        assert_eq!(js.matches("j.manualRender(").count(), 2, "{js}");
        assert_eq!(
            js.matches("await within(j.manualRender(frame(), true))")
                .count(),
            2,
            "{js}"
        );
        assert_eq!(js.matches("tick()").count(), 2, "{js}");
        assert_eq!(js.matches("await within(tick())").count(), 2, "{js}");
        // A hidden page runs no animation frames; a timer must still end a tick.
        assert!(
            js.contains(&format!("setTimeout(ok, {STYLED_TICK_FALLBACK_MS})")),
            "{js}"
        );
        assert!(js.contains("paused_redraw=ok"));
        assert!(js.contains(r#"return "failed: paused redraw ""#));
        assert!(js.contains("if (!owned) made.forEach((u) => URL.revokeObjectURL(u))"));
        assert!(styled_stop_js(3).contains("r.ro.disconnect()"));
        assert_eq!(
            parse_bridge(&serde_json::json!({"t": "styledresize", "run": 3})),
            Some(Bridge::StyledResize { run: 3 })
        );
    }

    #[test]
    fn the_styled_lift_measures_the_canvas_against_the_bar_and_sets_the_var() {
        assert!(STYLED_LIFT_MEASURE_JS.contains("canvas.JASSUB"));
        assert!(STYLED_LIFT_MEASURE_JS.contains(".pharos-controls"));
        assert!(STYLED_LIFT_MEASURE_JS.contains("bar.offsetHeight"));
        assert!(styled_lift_set_js(108).contains(r#"setProperty("--pharos-ass-lift", "108")"#));
    }

    #[test]
    fn a_stopped_styled_run_is_destroyed_or_marked_gone() {
        let js = styled_stop_js(7);
        assert!(js.contains("runs[7]"));
        assert!(js.contains("r.j.destroy()"));
        assert!(js.contains(r#"runs[7] = "gone""#));
    }

    #[test]
    fn the_styled_renderers_frames_ride_on_the_media_message() {
        let with = serde_json::json!({"t": "media", "id": "v", "pos": 1.0, "dur": 2.0,
            "paused": false, "ended": false, "seeking": false, "ready": 4, "vol": 1.0,
            "muted": false, "sub_skipped": 3, "sub_drawn": 90});
        let Some(Bridge::Media(with)) = parse_bridge(&with) else {
            panic!("not media");
        };
        assert_eq!((with.sub_skipped, with.sub_drawn), (Some(3), Some(90)));
        let without = serde_json::json!({"t": "media", "id": "v", "pos": 1.0, "dur": 2.0,
            "paused": false, "ended": false, "seeking": false, "ready": 4, "vol": 1.0,
            "muted": false, "sub_skipped": null});
        let Some(Bridge::Media(without)) = parse_bridge(&without) else {
            panic!("not media");
        };
        assert_eq!((without.sub_skipped, without.sub_drawn), (None, None));
    }

    #[test]
    fn a_jump_scrolls_the_grid_tile_and_focuses_it() {
        let js = scroll_to_tile_js(42, 9);
        assert!(
            js.contains(r#"document.querySelector(".pharos-browse .pharos-grid")"#),
            "{js}"
        );
        assert!(js.contains(r#"[data-at="42"]"#), "{js}");
        assert!(js.contains("window.__pharosGridSeq = 9;"), "{js}");
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
        assert_eq!(bridge_script("__OVERLAYS__"), OVERLAYS);
        // B1330: the seek bar is a remote stop that scrubs.
        assert_eq!(bridge_script("__REMOTE_SLIDERS__"), REMOTE_SLIDERS);
        assert_eq!(
            REMOTE_SLIDERS,
            format!("#{}", crate::native::watch_view::SEEK_ID)
        );
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

    /// B1404, V1374: an arrow on a poster tile or a jump letter reaches the
    /// bridge from the capture phase and never reaches Dioxus's root
    /// listener, whose synchronous round trip per key queued held repeats.
    #[test]
    fn an_arrow_on_a_tile_skips_the_vdom_round_trip() {
        let fast = bridge_script("__ARROWS_SKIP_VDOM__");
        assert!(!fast.contains("__"), "{fast}");
        assert!(
            fast.contains(&format!("t.matches('{PAGE_ONLY_ARROWS}')")),
            "{fast}"
        );
        assert!(
            fast.contains("a.pharos-poster") && fast.contains("button.pharos-letter"),
            "{fast}"
        );
        // Only arrows, and not inside a modal dialog, whose own handlers
        // (Escape, focus traps) still have to hear its keys.
        assert!(fast.contains(r#"!e.key.startsWith("Arrow")"#), "{fast}");
        assert!(
            fast.contains(r#"t.closest("[aria-modal='true']")"#),
            "{fast}"
        );
        // The bridge hears the key once, before the stop, and in the
        // capture phase, ahead of the root listener.
        let (Some(send), Some(stop)) = (
            fast.find("bridgeKey(e);"),
            fast.find("e.stopPropagation();"),
        ) else {
            panic!("no bridge send or stop: {fast}");
        };
        assert!(send < stop, "{fast}");
        assert!(fast.trim_end().ends_with("}, true);"), "{fast}");
    }

    /// B1340, V1311: on the TV, OK on a tile that plays on a hold opens its
    /// title page, and a hold presses its undrawn play link.
    #[test]
    fn ok_on_a_play_at_once_tile_opens_its_title_and_a_hold_plays() {
        let hold = bridge_script("__TILE_HOLD__");
        assert!(!hold.contains("__"), "{hold}");
        assert!(hold.contains(&format!("}}, {TILE_HOLD_MS});")), "{hold}");
        assert!(hold.contains("a.pharos-poster-play"), "{hold}");
        let Some((held, press)) = hold.split_once(r#"addEventListener("keyup""#) else {
            panic!("no keyup handler: {hold}");
        };
        assert!(
            held.contains("at.play.click();") && !held.contains("at.tile.click();"),
            "a hold plays: {held}"
        );
        assert!(
            press.contains("at.tile.click();") && !press.contains("at.play.click();"),
            "a press opens the title page: {press}"
        );
        // An OK released after a full hold plays, even when the hold's
        // timer has not run yet.
        assert!(
            press.contains(&format!(
                "if (held >= {TILE_HOLD_MS}) {{ holdPlay(at, \"release\"); return; }}"
            )),
            "{press}"
        );
        assert!(held.contains(r#"holdPlay(at, "timer");"#), "{held}");
        // The press is timed from the key events' own times, not from when
        // a busy page ran their handlers.
        assert!(held.contains("at.started = e.timeStamp;"), "{held}");
        assert!(
            press.contains("const held = e.timeStamp - at.started;"),
            "{press}"
        );
        // A hold dropped by a focus or page change neither plays nor opens.
        for drop in [
            r#"addEventListener("focusout""#,
            r#"addEventListener("blur", dropHold)"#,
            "visibilitychange",
        ] {
            assert!(held.contains(drop), "{drop}: {held}");
        }
        assert!(
            held.contains("if (tileHold !== at || at.done) return;"),
            "{held}"
        );
        for verdict in ["verdict=play", "verdict=open"] {
            assert!(hold.contains(verdict), "{verdict} logged: {hold}");
        }
    }

    #[test]
    fn base64_matches_the_standard_vectors() {
        for (raw, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(raw.as_bytes()), enc, "{raw}");
        }
        assert_eq!(base64(&[0xFB, 0xFF, 0xFE]), "+//+");
    }

    #[test]
    fn a_host_frame_is_pushed_as_bytes_with_the_pages_own_stamp() {
        let frame = super::super::ports::VisFrame {
            sample_rate: 44_100,
            bins: vec![0, 255],
            wave: vec![127, -127],
        };
        let js = visualiser_frame_js(&frame);
        assert!(js.contains("rate: 44100"), "{js}");
        assert!(js.contains("at: performance.now()"), "{js}");
        assert!(js.contains(r#"bins: d("AP8=")"#), "{js}");
        assert!(
            js.contains(r#"d("f4E=")"#),
            "i8 127, -127 are bytes 7f 81 -> {js}"
        );
    }

    #[test]
    fn only_a_host_visualiser_is_told_to_read_pushed_frames() {
        let plain = visualiser_start_for_js(3, "plasma", false);
        assert_eq!(plain, visualiser_start_js(3, "plasma"));
        assert!(!plain.contains("__pharosVisHost"));
        let hosted = visualiser_start_for_js(3, "plasma", true);
        assert!(
            hosted.starts_with("window.__pharosVisHost = true;"),
            "{hosted}"
        );
        assert!(
            hosted.ends_with(plain.as_str()) || hosted.contains(&plain),
            "{hosted}"
        );
    }

    #[test]
    fn the_pump_pushes_frames_then_one_silent_frame_then_nothing() {
        use super::super::ports::VisFrame;
        let mut last = None;
        let frame = VisFrame {
            sample_rate: 44_100,
            bins: vec![1],
            wave: vec![1],
        };
        let (sent, wait) = next_push(&mut last, Some(frame.clone()));
        assert_eq!((sent, wait), (Some(frame), FRAME_PUSH_MS));
        let (sent, wait) = next_push(&mut last, None);
        assert_eq!(sent, Some(VisFrame::silent(44_100)), "falls quiet once");
        assert_eq!(wait, FRAME_IDLE_MS);
        assert_eq!(next_push(&mut last, None), (None, FRAME_IDLE_MS));
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
    fn a_proxy_error_logs_every_cause() {
        #[derive(Debug)]
        struct Layer(&'static str, Option<Box<Layer>>);
        impl std::fmt::Display for Layer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.0)
            }
        }
        impl std::error::Error for Layer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                self.1.as_deref().map(|e| e as _)
            }
        }
        let error = Layer(
            "error sending request",
            Some(Box::new(Layer("connection reset", None))),
        );
        assert_eq!(
            error_chain(&error),
            "error sending request: connection reset"
        );
        assert_eq!(error_chain(&Layer("alone", None)), "alone");
    }

    #[test]
    fn a_proxied_route_logs_as_a_bounded_label() {
        assert_eq!(
            proxy_segment("/videos/1/subtitles/2/images/9.png"),
            "subtitle_image"
        );
        assert_eq!(proxy_segment("/Items/1/Images/Primary?tag=a"), "Items");
        assert_eq!(proxy_segment("/ui/assets/x.js"), "ui");
        assert_eq!(proxy_segment("/nope/1"), "other");
        assert_eq!(proxy_segment(""), "other");
    }

    #[test]
    fn the_phone_loads_artwork_through_the_proxy_and_the_tv_direct() {
        let art = "/Items/1/Images/Primary?tag=a";
        assert_eq!(art_src_for(Shell::AndroidPhone, "https://s", art), art);
        assert!(proxied(art.split('/').nth(1).unwrap_or_default()));
        assert_eq!(
            art_src_for(Shell::AndroidTv, "https://s", art),
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
    fn an_elements_width_comes_in_from_the_page() {
        assert_eq!(
            parse_bridge(&serde_json::json!({"t": "width", "id": "pharos-seek", "w": 312.5})),
            Some(Bridge::Width {
                id: "pharos-seek".into(),
                w: 312.5
            })
        );
        let script = bridge_script("__MEASURE__");
        assert!(!script.contains("__"), "{script}");
        assert!(script.contains(&format!("[{MEASURE_ATTR}]")), "{script}");
        assert!(script.contains("ResizeObserver"), "{script}");
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
        assert_eq!(
            parse_bridge(&serde_json::json!({"t": "backclose", "what": "menu", "restore": true})),
            Some(Bridge::BackClose {
                what: "menu".to_owned(),
                restore: true
            })
        );
        assert_eq!(
            parse_bridge(
                &serde_json::json!({"t": "place", "path": "/ui/browse/1", "tile": 40, "top": 212.5})
            ),
            Some(Bridge::Place {
                path: "/ui/browse/1".to_owned(),
                tile: 40,
                top: 212.5
            })
        );
        assert_eq!(
            parse_bridge(
                &serde_json::json!({"t": "grid", "path": "/ui/browse/1", "top": -900.0,
                "height": 540.0, "cols": 9, "pitch": 151.5, "gap": 13.6, "seq": 2})
            ),
            Some(Bridge::Grid {
                path: "/ui/browse/1".to_owned(),
                view: super::super::grid_window::GridView {
                    top: -900.0,
                    height: 540.0,
                    cols: 9,
                    pitch: 151.5,
                    gap: 13.6,
                    seq: 2
                }
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
        assert_eq!(
            (m.ev.as_str(), m.ns, m.fwd, m.buffered.as_str()),
            ("", 0, 0, ""),
            "an old message names none of the trace's fields"
        );
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

    /// B1386 (V1356): the TV's seek bar draws the buffered range from the
    /// play head the page reported, and none in a gap.
    #[test]
    fn the_buffered_range_ends_where_the_page_said_data_runs_out() {
        let v = serde_json::json!({"t": "media", "id": "v", "pos": 10.0, "dur": 20.0,
            "paused": true, "ended": false, "seeking": false, "ready": 4, "vol": 1.0,
            "muted": false, "ev": "progress", "fwd": 6500});
        let Some(Bridge::Media(m)) = parse_bridge(&v) else {
            panic!("not media");
        };
        assert_eq!(m.buffered_to_ms(), 16_500);
        let gap = Media { fwd: 0, ..m };
        assert_eq!(
            gap.buffered_to_ms(),
            0,
            "a gap at the play head draws no range"
        );
        assert_eq!(
            signal_of("progress", false),
            None,
            "a progress is not a trace signal"
        );
    }

    /// B1367: the page sends the event type, network state and forward
    /// buffer the play's trace needs.
    #[test]
    fn a_media_event_carries_what_the_play_trace_reads() {
        let v = serde_json::json!({"t": "media", "id": "v", "pos": 10.0, "dur": 20.0,
            "paused": false, "ended": false, "seeking": false, "ready": 2, "vol": 1.0,
            "muted": false, "ev": "waiting", "ns": 2, "fwd": 0,
            "buffered": "0-9000,12000-15000"});
        let Some(Bridge::Media(m)) = parse_bridge(&v) else {
            panic!("not media");
        };
        assert_eq!(
            (m.ev.as_str(), m.ns, m.fwd, m.buffered.as_str()),
            ("waiting", 2, 0, "0-9000,12000-15000")
        );
    }

    /// B1367, V1337: the media events the trace reads, and only those.
    #[test]
    fn the_trace_reads_playing_waiting_seeks_pause_and_ticks() {
        assert_eq!(signal_of("playing", false), Some(Signal::Playing));
        assert_eq!(signal_of("waiting", false), Some(Signal::Waiting));
        assert_eq!(signal_of("seeking", false), Some(Signal::Seeking));
        assert_eq!(signal_of("seeked", false), Some(Signal::Seeked));
        assert_eq!(signal_of("pause", false), Some(Signal::Pause));
        assert_eq!(signal_of("timeupdate", true), Some(Signal::TimeUpdate));
        for ignored in ["play", "canplay", "volumechange", "ended", "emptied", ""] {
            assert_eq!(signal_of(ignored, false), None, "{ignored}");
        }
        assert_eq!(signal_of("timeupdate", false), None, "a throttled-out one");
        assert_eq!(signal_of("", true), None, "a tick with no event type");
    }

    #[test]
    fn a_clip_carries_the_media_state_the_trace_reads() {
        let v = serde_json::json!({"t": "media", "id": "v", "pos": 12.3456, "dur": 20.0,
            "paused": true, "ended": false, "seeking": false, "ready": 2, "vol": 1.0,
            "muted": false, "ev": "waiting", "ns": 2, "fwd": 40, "buffered": "0-12400"});
        let Some(Bridge::Media(m)) = parse_bridge(&v) else {
            panic!("not media");
        };
        assert_eq!(
            clip_of(&m, 9_000),
            Clip {
                now_ms: 9_000,
                position_ms: 12_346,
                paused: true,
                ready_state: 2,
                network_state: 2,
                forward_ms: 40,
                buffered: "0-12400".into(),
            }
        );
    }

    #[test]
    fn a_fragment_is_an_appended_segment_on_its_own_track() {
        let video = appended_of(7, 1_000, 30, 200, FragTrack::Main);
        assert_eq!(
            (
                video.track,
                video.index,
                video.bytes,
                video.ttfb_ms,
                video.load_ms
            ),
            (Track::Video, 7, 1_000, 30, 200)
        );
        assert_eq!(
            appended_of(7, 1, 1, 1, FragTrack::Audio).track,
            Track::Audio
        );
    }

    #[test]
    fn a_key_says_when_the_focus_is_on_a_field() {
        let key = serde_json::json!({"t": "key", "key": "ArrowDown", "field": true});
        let Some(Bridge::Key { at, text, .. }) = parse_bridge(&key) else {
            panic!("not a key");
        };
        assert!(at.field);
        assert!(!text, "an arrow leaving a field is not a key in it");
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
        assert_eq!(
            parse_bridge(&f),
            Some(Bridge::Hls {
                id: "v".into(),
                event: HlsEvent::Frag {
                    bytes: 10,
                    load_ms: 2,
                    media_ms: 4000,
                    sn: 0,
                    ttfb_ms: 0,
                    track: FragTrack::Main,
                }
            }),
            "an old message is a main fragment with no number"
        );
    }

    /// B1367: a fragment says which rendition it is, its number and its
    /// time to first byte.
    #[test]
    fn a_fragment_names_its_track_number_and_first_byte() {
        let f = serde_json::json!({"t": "hls", "id": "v", "ev": "frag",
            "bytes": 10, "load_ms": 2, "media_ms": 4000,
            "sn": 7, "ttfb_ms": 35, "track": "audio"});
        assert_eq!(
            parse_bridge(&f),
            Some(Bridge::Hls {
                id: "v".into(),
                event: HlsEvent::Frag {
                    bytes: 10,
                    load_ms: 2,
                    media_ms: 4000,
                    sn: 7,
                    ttfb_ms: 35,
                    track: FragTrack::Audio,
                }
            })
        );
    }

    /// B1367: the play trace counts a segment once it is appended, which
    /// hls.js says with `FRAG_BUFFERED`, not when the download ends.
    #[test]
    fn a_buffered_fragment_parses_with_defaults() {
        let b = serde_json::json!({"t": "hls", "id": "v", "ev": "buffered",
            "bytes": 10, "load_ms": 2, "sn": 7, "ttfb_ms": 35, "track": "audio"});
        assert_eq!(
            parse_bridge(&b),
            Some(Bridge::Hls {
                id: "v".into(),
                event: HlsEvent::Buffered {
                    bytes: 10,
                    load_ms: 2,
                    sn: 7,
                    ttfb_ms: 35,
                    track: FragTrack::Audio,
                }
            })
        );
        let bare = serde_json::json!({"t": "hls", "id": "v", "ev": "buffered"});
        assert_eq!(
            parse_bridge(&bare),
            Some(Bridge::Hls {
                id: "v".into(),
                event: HlsEvent::Buffered {
                    bytes: 0,
                    load_ms: 0,
                    sn: 0,
                    ttfb_ms: 0,
                    track: FragTrack::Main,
                }
            })
        );
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
