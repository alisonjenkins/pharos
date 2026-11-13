//! The episode turn (V790) as the Android TV app's page runs it (B1412,
//! V1382). The TV's Rust runs outside the WebView, where the `<video>` and
//! the DOM are, so the layer `turn_web.rs` builds with `web_sys` is built
//! here by a page script instead: the same elements and classes (so the
//! same `pharos.css` rules draw it), the same rules from `turn.rs` written
//! into the script as numbers and words, and the same log lines. Rust only
//! says when: [`call_js`] for each of the platform's `turn_*` calls.
//!
//! Before this the TV's `turn_*` were no-ops, so opening a video showed
//! the first load's "Back" pill until the player came, where the browser
//! turned onto the video.

use super::turn::{
    caption, progress, Direction, Step, BUFFER_FULL_MS, CAPTION_AFTER_MS, FADE_EASE, FADE_MS,
    GIVE_UP_MS, SNAPSHOT_MAX_WIDTH, STALLED_AFTER_MS, START_DEG, TICK_MS, TURNING_ATTR, TURN_EASE,
    TURN_MS,
};
use super::watch_view::VIDEO_ID;

/// The page's turn: `window.pharosTurn`, with `expect`, `begin`, `open`,
/// `ready`, `cancel` and `cancelCurrent`, the browser's `turn_*` calls by
/// name. Installed once at launch beside the bridge, not inside it: the E2E
/// harness (`tv-bridge.ts`) rebuilds the bridge from constants alone.
/// Placeholders are filled by [`turn_script`].
const TURN_JS: &str = r#"
window.pharosTurn = (() => {
  const TURN_MS = __TURN_MS__, FADE_MS = __FADE_MS__, GIVE_UP_MS = __GIVE_UP_MS__;
  const START_DEG = __START_DEG__, TICK_MS = __TICK_MS__, SNAP_W = __SNAP_W__;
  const TURN_EASE = __TURN_EASE__, FADE_EASE = __FADE_EASE__;
  const VIDEO_ID = __VIDEO_ID__, TURNING = __TURNING__;
  const DONE = { clicked: 0, negotiated: __DONE_NEGOTIATED__, metadata: __DONE_METADATA__, first: 1 };
  const BUFFERED_MAX = __DONE_BUFFERED_MAX__, BUFFER_FULL_MS = __BUFFER_FULL_MS__;
  const CAPTION_AFTER_MS = __CAPTION_AFTER_MS__, STALLED_AFTER_MS = __STALLED_AFTER_MS__;
  const CAPTIONS = { clicked: __CAP_CLICKED__, negotiated: __CAP_NEGOTIATED__, buffering: __CAP_BUFFERING__, stalled: __CAP_STALLED__ };
  let active = null, expected = null;
  const log = (m) => console.info("pharos: turn " + m);
  const face = (deg) => "translateZ(-50vw) rotateY(" + deg.toFixed(2) + "deg) translateZ(50vw)";
  const setFace = (f, deg, ms) => {
    f.style.transition = ms == null ? "none" : "transform " + ms + "ms " + TURN_EASE;
    f.style.transform = face(deg);
  };
  const el = (tag, cls) => { const e = document.createElement(tag); e.className = cls; return e; };
  const video = () => {
    const v = document.getElementById(VIDEO_ID);
    return v && v.tagName === "VIDEO" ? v : null;
  };
  const progress = (s) => s.kind === "buffered"
    ? DONE.metadata + (BUFFERED_MAX - DONE.metadata) * Math.min(s.ms, BUFFER_FULL_MS) / BUFFER_FULL_MS
    : DONE[s.kind];
  const caption = (a, now) => {
    if (now - a.begun < CAPTION_AFTER_MS || a.step.kind === "first") return null;
    if (now - a.stepped >= STALLED_AFTER_MS) return CAPTIONS.stalled;
    if (a.step.kind === "clicked") return CAPTIONS.clicked;
    if (a.step.kind === "negotiated") return CAPTIONS.negotiated;
    return CAPTIONS.buffering;
  };
  // The old episode's frame, no wider than SNAP_W: cheap from a 4K frame.
  const snapshot = (v) => {
    try {
      const w = Math.min(v.videoWidth, SNAP_W);
      const h = Math.max(1, Math.round(v.videoHeight * w / Math.max(1, v.videoWidth)));
      const c = el("canvas", "pharos-turn-still");
      c.width = w; c.height = h;
      c.getContext("2d").drawImage(v, 0, 0, w, h);
      return c;
    } catch (e) {
      console.warn("pharos: turn snapshot failed error=" + e);
      return null;
    }
  };
  const finish = (reason) => {
    const a = active;
    if (!a) return;
    active = null;
    a.layer.remove();
    document.documentElement.removeAttribute(TURNING);
    clearInterval(a.tick);
    clearTimeout(a.turnTimer);
    clearTimeout(a.fadeTimer);
    for (const [at, kind, f, capture] of a.listeners) at.removeEventListener(kind, f, capture);
    log("end item=" + a.item + " reason=" + reason + " after_ms=" + Math.round(performance.now() - a.begun));
  };
  const fadeOut = (a) => {
    if (active !== a || a.fading) return;
    a.fading = true;
    a.layer.style.transition = "opacity " + FADE_MS + "ms " + FADE_EASE;
    a.layer.style.opacity = "0";
    a.layer.classList.remove("is-waiting");
    a.cap.classList.remove("is-shown");
    log("fade item=" + a.item + " after_ms=" + Math.round(performance.now() - a.begun));
    a.fadeTimer = setTimeout(() => { if (active === a) finish("landed"); }, FADE_MS);
  };
  const hear = (a, step) => {
    if (active !== a || a.fading) return;
    const to = progress(step);
    if (!(to > progress(a.step))) return;
    const now = performance.now();
    a.step = step;
    a.stepped = now;
    a.fill.style.transform = "scaleX(" + to.toFixed(3) + ")";
    // A quick load still finishes the turn before it fades (B841).
    if (step.kind === "first" && now - a.begun >= TURN_MS) fadeOut(a);
  };
  // The one move is over: fade if the first frame is in, else show the wait.
  const turned = (a) => {
    if (active !== a || a.fading) return;
    if (a.step.kind === "first") { fadeOut(a); return; }
    a.layer.classList.add("is-waiting");
  };
  const tick = (a) => {
    if (active !== a || a.fading) return;
    const now = performance.now();
    if (now - a.begun >= GIVE_UP_MS) {
      console.warn("pharos: turn given up item=" + a.item + " after_ms=" + GIVE_UP_MS + " reason=no-first-frame");
      finish("gave-up");
      return;
    }
    const text = caption(a, now);
    if (text) {
      if (a.cap.textContent !== text) a.cap.textContent = text;
      a.cap.classList.add("is-shown");
    } else {
      a.cap.classList.remove("is-shown");
    }
  };
  const ahead = (v) => {
    const r = v.buffered, at = v.currentTime;
    for (let i = 0; i < r.length; i++) {
      if (r.start(i) <= at + 0.5 && at <= r.end(i)) return Math.max(0, (r.end(i) - at) * 1000);
    }
    return 0;
  };
  const attach = (a) => {
    const v = video();
    if (!v) return;
    a.video = v;
    const on = {
      loadedmetadata: () => hear(a, { kind: "metadata" }),
      loadeddata: () => hear(a, { kind: "first" }),
      error: () => { if (active === a) finish("video-error"); },
    };
    for (const kind in on) {
      v.addEventListener(kind, on[kind], false);
      a.listeners.push([v, kind, on[kind], false]);
    }
    // The bridge stops `progress` at the document, in the capture phase, so
    // it never reaches the video; a capturing listener on the document
    // itself still hears it.
    const progress = (e) => { if (e.target === v) hear(a, { kind: "buffered", ms: ahead(v) }); };
    document.addEventListener("progress", progress, true);
    a.listeners.push([document, "progress", progress, true]);
    // Events that fired before the hook.
    if (v.readyState >= 2) on.loadeddata();
    else if (v.readyState >= 1) on.loadedmetadata();
  };
  const start = (item, sign, outgoing, from) => {
    const fs = document.fullscreenElement;
    if (fs && fs !== document.documentElement) {
      log("skipped item=" + item + " reason=player-fullscreen");
      return;
    }
    if (active) finish("replaced");
    const layer = el("div", "pharos-turn");
    let out = null;
    if (outgoing) {
      out = el("div", "pharos-turn-face is-out");
      out.append(outgoing, el("div", "pharos-turn-shade"));
      layer.append(out);
    }
    const f = el("div", "pharos-turn-face is-in");
    const still = el("img", "pharos-turn-still");
    still.alt = "";
    const title = el("p", "pharos-turn-title");
    const cap = el("p", "pharos-turn-caption");
    cap.setAttribute("role", "status");
    cap.setAttribute("aria-live", "polite");
    const bar = el("div", "pharos-turn-bar"), fill = el("div", "pharos-turn-fill");
    bar.append(fill);
    f.append(still, el("div", "pharos-turn-shade"), el("div", "pharos-turn-rim"), title, cap, bar);
    layer.append(f);
    document.body.append(layer);
    document.documentElement.setAttribute(TURNING, "");
    // Into the first pose without a transition, then the whole turn in one
    // move; the load never holds it part way (B841).
    if (out) setFace(out, 0, null);
    setFace(f, START_DEG * sign, null);
    void layer.offsetWidth;
    if (out) { setFace(out, -START_DEG * sign, TURN_MS); out.classList.add("is-gone"); }
    setFace(f, 0, TURN_MS);
    const now = performance.now();
    const a = {
      item, layer, fill, cap, title, still, step: { kind: "clicked" }, begun: now, stepped: now,
      fading: false, video: null, listeners: [], tick: 0, turnTimer: 0, fadeTimer: 0,
    };
    active = a;
    a.tick = setInterval(() => tick(a), TICK_MS);
    a.turnTimer = setTimeout(() => turned(a), TURN_MS);
    log("begin item=" + item + " from=" + from + " dir=" + (sign > 0 ? "forward" : "back"));
  };
  return {
    // A navigation opens `item`: turn onto it once its detail says it is a video.
    expect(item, sign) { expected = { item, sign }; },
    // From the episode on screen: only a video with a frame showing turns away.
    begin(item, sign) {
      const v = video();
      const snap = v && v.readyState >= 2 && v.videoWidth > 0 ? snapshot(v) : null;
      if (snap) start(item, sign, snap, "next");
      else expected = { item, sign };
    },
    open(item) {
      if (!expected || expected.item !== item) return;
      const { sign } = expected;
      expected = null;
      start(item, sign, null, "open");
    },
    ready(item, image, title) {
      const a = active;
      if (!a || a.item !== item) return;
      if (image) a.still.setAttribute("src", image);
      a.title.textContent = title;
      hear(a, { kind: "negotiated" });
      attach(a);
    },
    cancel(item, reason) {
      if (expected && expected.item === item) expected = null;
      if (active && active.item === item) finish(reason);
    },
    cancelCurrent(reason) { finish(reason); },
  };
})();
"#;

/// `value` as a JavaScript string literal.
fn js_str(value: &str) -> String {
    serde_json::Value::String(value.to_owned()).to_string()
}

/// The caption `turn.rs` gives a load at `step` that has waited long, or
/// stalled.
fn caption_for(step: Step, stalled: bool) -> String {
    let since_step = if stalled { STALLED_AFTER_MS } else { 0 };
    js_str(caption(step, CAPTION_AFTER_MS, since_step).unwrap_or_default())
}

/// [`TURN_JS`] with `turn.rs`'s rules written in, for the page's bridge.
pub fn turn_script() -> String {
    TURN_JS
        .replace("__TURN_MS__", &TURN_MS.to_string())
        .replace("__FADE_MS__", &FADE_MS.to_string())
        .replace("__GIVE_UP_MS__", &GIVE_UP_MS.to_string())
        .replace("__START_DEG__", &START_DEG.to_string())
        .replace("__TICK_MS__", &TICK_MS.to_string())
        .replace("__SNAP_W__", &SNAPSHOT_MAX_WIDTH.to_string())
        .replace("__TURN_EASE__", &js_str(TURN_EASE))
        .replace("__FADE_EASE__", &js_str(FADE_EASE))
        .replace("__VIDEO_ID__", &js_str(VIDEO_ID))
        .replace("__TURNING__", &js_str(TURNING_ATTR))
        .replace(
            "__DONE_NEGOTIATED__",
            &progress(Step::Negotiated).to_string(),
        )
        .replace("__DONE_METADATA__", &progress(Step::Metadata).to_string())
        .replace(
            "__DONE_BUFFERED_MAX__",
            &progress(Step::Buffered(BUFFER_FULL_MS)).to_string(),
        )
        .replace("__BUFFER_FULL_MS__", &BUFFER_FULL_MS.to_string())
        .replace("__CAPTION_AFTER_MS__", &CAPTION_AFTER_MS.to_string())
        .replace("__STALLED_AFTER_MS__", &STALLED_AFTER_MS.to_string())
        .replace("__CAP_CLICKED__", &caption_for(Step::Clicked, false))
        .replace("__CAP_NEGOTIATED__", &caption_for(Step::Negotiated, false))
        .replace("__CAP_BUFFERING__", &caption_for(Step::Metadata, false))
        .replace("__CAP_STALLED__", &caption_for(Step::Clicked, true))
}

/// One of the platform's `turn_*` calls, as the page script that makes it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Call<'a> {
    Expect(&'a str, Direction),
    Begin(&'a str, Direction),
    Open(&'a str),
    Ready {
        item: &'a str,
        image: Option<&'a str>,
        title: &'a str,
    },
    Cancel(&'a str, &'a str),
    CancelCurrent(&'a str),
}

/// The script that makes `call`; nothing happens if the bridge (and with it
/// the turn) is not in the page.
pub fn call_js(call: Call<'_>) -> String {
    let sign = |d: Direction| if d == Direction::Forward { "1" } else { "-1" };
    let (method, args) = match call {
        Call::Expect(item, dir) => ("expect", format!("{}, {}", js_str(item), sign(dir))),
        Call::Begin(item, dir) => ("begin", format!("{}, {}", js_str(item), sign(dir))),
        Call::Open(item) => ("open", js_str(item)),
        Call::Ready { item, image, title } => (
            "ready",
            format!(
                "{}, {}, {}",
                js_str(item),
                image.map_or_else(|| "null".to_owned(), js_str),
                js_str(title)
            ),
        ),
        Call::Cancel(item, reason) => ("cancel", format!("{}, {}", js_str(item), js_str(reason))),
        Call::CancelCurrent(reason) => ("cancelCurrent", js_str(reason)),
    };
    format!("if (window.pharosTurn) window.pharosTurn.{method}({args});")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::super::turn::face_transform;
    use super::*;

    /// V1382: every rule `turn.rs` sets is written into the TV's script; a
    /// placeholder left over would be a syntax error in the page.
    #[test]
    fn the_tv_turn_carries_every_rule_and_no_placeholder() {
        let js = turn_script();
        assert!(!js.contains("__"), "placeholder left: {js}");
        for want in [
            format!("TURN_MS = {TURN_MS},"),
            format!("FADE_MS = {FADE_MS},"),
            format!("GIVE_UP_MS = {GIVE_UP_MS};"),
            format!("TICK_MS = {TICK_MS},"),
            format!("SNAP_W = {SNAPSHOT_MAX_WIDTH};"),
            format!("VIDEO_ID = {}", js_str(VIDEO_ID)),
            "\"Starting playback…\"".to_owned(),
            "\"Opening the stream…\"".to_owned(),
            "\"Buffering…\"".to_owned(),
            "\"Waiting for the server…\"".to_owned(),
        ] {
            assert!(js.contains(&want), "{want} missing");
        }
    }

    /// The page script poses a face exactly as `face_transform` does.
    #[test]
    fn the_tv_turn_poses_faces_as_the_browser_does() {
        let js = turn_script();
        let flat = face_transform(0.0);
        let (head, tail) = flat
            .split_once("0.00deg")
            .expect("degrees in the transform");
        assert!(
            js.contains(&format!("\"{head}\" + deg.toFixed(2) + \"deg{tail}\"")),
            "{js}"
        );
    }

    /// The layer uses the classes `pharos.css` draws, which `turn_web.rs`
    /// builds too.
    #[test]
    fn the_tv_turn_builds_the_styled_layer() {
        let js = turn_script();
        let css = include_str!("../../assets/pharos.css");
        for class in [
            "pharos-turn",
            "pharos-turn-face is-in",
            "pharos-turn-face is-out",
            "pharos-turn-still",
            "pharos-turn-shade",
            "pharos-turn-rim",
            "pharos-turn-title",
            "pharos-turn-caption",
            "pharos-turn-bar",
            "pharos-turn-fill",
        ] {
            assert!(js.contains(&format!("\"{class}\"")), "{class} not built");
            let first = class.split(' ').next().unwrap_or_default();
            assert!(css.contains(&format!(".{first}")), "{first} not styled");
        }
    }

    /// The bridge stops `progress` at the document in the capture phase
    /// (B1386), so the turn hears it there too, or its bar would never
    /// fill past the metadata step.
    #[test]
    fn the_tv_turn_hears_download_progress_where_the_bridge_stops_it() {
        let js = turn_script();
        assert!(
            js.contains(r#"document.addEventListener("progress", progress, true);"#),
            "{js}"
        );
        assert!(!js.contains("progress: () =>"), "{js}");
    }

    #[test]
    fn each_platform_call_becomes_a_guarded_page_call() {
        assert_eq!(
            call_js(Call::Expect("ab", Direction::Forward)),
            "if (window.pharosTurn) window.pharosTurn.expect(\"ab\", 1);"
        );
        assert_eq!(
            call_js(Call::Begin("ab", Direction::Back)),
            "if (window.pharosTurn) window.pharosTurn.begin(\"ab\", -1);"
        );
        assert_eq!(
            call_js(Call::Open("ab")),
            "if (window.pharosTurn) window.pharosTurn.open(\"ab\");"
        );
        assert_eq!(
            call_js(Call::Ready {
                item: "ab",
                image: None,
                title: "A \"quoted\" name"
            }),
            "if (window.pharosTurn) window.pharosTurn.ready(\"ab\", null, \"A \\\"quoted\\\" name\");"
        );
        assert_eq!(
            call_js(Call::Ready {
                item: "ab",
                image: Some("/p.jpg"),
                title: "t"
            }),
            "if (window.pharosTurn) window.pharosTurn.ready(\"ab\", \"/p.jpg\", \"t\");"
        );
        assert_eq!(
            call_js(Call::Cancel("ab", "left")),
            "if (window.pharosTurn) window.pharosTurn.cancel(\"ab\", \"left\");"
        );
        assert_eq!(
            call_js(Call::CancelCurrent("engine-fatal")),
            "if (window.pharosTurn) window.pharosTurn.cancelCurrent(\"engine-fatal\");"
        );
    }
}
