//! 025 slice 0 spike: the Android TV app. One screen: QuickConnect sign-in,
//! one home row, a muted hls.js player, an on-screen key log and the
//! WebView's codec answers. Throwaway: it proves `dx` Android on the TV.
//!
//!     nix develop .#android -c dx bundle --platform android \
//!         --package pharos-ui --bin pharos-tv --features mobile \
//!         --target armv7-linux-androideabi

#[cfg(not(target_os = "android"))]
fn main() {
    eprintln!("pharos-tv only runs on Android");
}

#[cfg(target_os = "android")]
fn main() {
    tv::main();
}

#[cfg(target_os = "android")]
mod tv {
    use dioxus::prelude::*;
    use futures_util::StreamExt;
    use pharos_client::api::{Capabilities, ItemKind, ItemRef, NativeApi, PharosApi, TrackPicks};
    use pharos_client::CallInfo;
    use pharos_ui::native::android;
    use std::sync::{Mutex, OnceLock};

    const BASE: &str = "https://prototype.aliflix.alison-jenkins.com";
    const HLS_JS: &str = include_str!("../../assets/vendor/hls.min.js");

    #[derive(Debug, Clone, PartialEq)]
    struct Tile {
        id: ItemRef,
        name: String,
        image: Option<String>,
    }

    enum Ev {
        Status(String),
        Code(String),
        Rail(String, Vec<Tile>),
        Play {
            url: String,
            hls: bool,
            method: String,
        },
    }

    enum Cmd {
        Play(ItemRef, Capabilities),
    }

    type EvRx = futures::channel::mpsc::UnboundedReceiver<Ev>;
    type CmdTx = futures::channel::mpsc::UnboundedSender<Cmd>;

    static EVENTS: OnceLock<Mutex<Option<EvRx>>> = OnceLock::new();
    static COMMANDS: OnceLock<CmdTx> = OnceLock::new();

    pub fn main() {
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Info)
                .with_tag("pharos-tv"),
        );
        let (ev_tx, ev_rx) = futures::channel::mpsc::unbounded();
        let (cmd_tx, cmd_rx) = futures::channel::mpsc::unbounded();
        let _ = EVENTS.set(Mutex::new(Some(ev_rx)));
        let _ = COMMANDS.set(cmd_tx);
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    log::error!("runtime: {e}");
                    return;
                }
            };
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                if let Err(e) = net(ev_tx.clone(), cmd_rx).await {
                    log::error!("net: {e}");
                    let _ = ev_tx.unbounded_send(Ev::Status(format!("error: {e}")));
                }
            });
        });
        dioxus::launch(App);
    }

    fn call() -> CallInfo {
        CallInfo {
            client_version: "pharos-android-tv/spike".into(),
            device: "pharos-android-tv".into(),
            ui_route: "/".into(),
            ..CallInfo::default()
        }
    }

    /// Same-origin server URL → absolute, carrying the device token, since
    /// the WebView's origin is not the server's and has no cookie.
    fn absolute(path: &str, token: &str) -> String {
        let sep = if path.contains('?') { '&' } else { '?' };
        format!("{BASE}{path}{sep}api_key={token}")
    }

    async fn net(
        ev: futures::channel::mpsc::UnboundedSender<Ev>,
        mut cmds: futures::channel::mpsc::UnboundedReceiver<Cmd>,
    ) -> Result<(), String> {
        let say = |s: String| {
            log::info!("{s}");
            let _ = ev.unbounded_send(Ev::Status(s));
        };
        say(format!("connecting to {BASE}"));
        let (boot, system, mut closed) = android::connect(BASE).await?;
        tokio::task::spawn_local(system);
        let info = call();

        let mut signed_in = None;
        if let Some(token) = android::load_token() {
            match android::resume(&boot, &info, &token).await {
                Ok(session) => {
                    say("resumed stored device token".into());
                    signed_in = Some((session, token));
                }
                Err(e) => {
                    say(format!("stored token refused: {e}"));
                    android::forget_token();
                }
            }
        }
        let (session, token) = match signed_in {
            Some(s) => s,
            None => {
                // The server closes a socket not signed in within 30 s
                // (`auth_deadline`) and rate-limits upgrades, so reconnect
                // only when the socket is gone. An expired code is replaced.
                let mut boot = boot;
                let mut secret = None::<String>;
                loop {
                    if closed.get() {
                        log::info!("socket closed while waiting; reconnecting");
                        let mut wait = 2;
                        (boot, closed) = loop {
                            match android::connect(BASE).await {
                                Ok((b, system, c)) => {
                                    tokio::task::spawn_local(system);
                                    break (b, c);
                                }
                                Err(e) if wait < 64 => {
                                    log::warn!("reconnect: {e}; retry in {wait} s");
                                    tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                                    wait *= 2;
                                }
                                Err(e) => return Err(e),
                            }
                        };
                    }
                    let Some(current) = secret.clone() else {
                        let (code, s) = android::quick_connect_initiate(&boot, &info)
                            .await
                            .map_err(|e| format!("quickConnectInitiate: {e}"))?;
                        log::info!("quick connect code {code}");
                        let _ = ev.unbounded_send(Ev::Code(code));
                        secret = Some(s);
                        continue;
                    };
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    let polled = match tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        android::quick_connect_poll(&boot, &info, &current),
                    )
                    .await
                    {
                        Ok(p) => p,
                        Err(_) => {
                            closed.set(true);
                            continue;
                        }
                    };
                    match polled {
                        Ok(Some((session, token))) => {
                            android::store_token(&token)?;
                            say("signed in via QuickConnect; token stored".into());
                            break (session, token);
                        }
                        Ok(None) => {}
                        Err(e) if e.is_disconnected() => closed.set(true),
                        Err(pharos_client::ClientError::Api {
                            kind: "notFound", ..
                        }) => {
                            log::info!("quick connect code expired; minting another");
                            secret = None;
                        }
                        Err(e) => return Err(format!("quickConnectPoll: {e}")),
                    }
                }
            }
        };

        let api = NativeApi::new(&session);
        let started = std::time::Instant::now();
        let home = api.home(&info).await.map_err(|e| format!("home: {e}"))?;
        say(format!(
            "home: {} rails in {} ms",
            home.rails.len(),
            started.elapsed().as_millis()
        ));
        let rail = home
            .rails
            .iter()
            .find(|r| r.latest.iter().any(|i| i.kind == ItemKind::Movie))
            .or(home.rails.first());
        if let Some(rail) = rail {
            let tiles = rail
                .latest
                .iter()
                .map(|i| Tile {
                    id: i.id.clone(),
                    name: i.name.clone(),
                    image: i.image.as_deref().map(|p| absolute(p, &token)),
                })
                .collect();
            let _ = ev.unbounded_send(Ev::Rail(rail.library.name.clone(), tiles));
        }

        let mut _playing = None;
        while let Some(cmd) = cmds.next().await {
            match cmd {
                Cmd::Play(id, caps) => {
                    log::info!("negotiate {id} caps {caps:?}");
                    match api.play(&info, id, &caps, TrackPicks::default()).await {
                        Ok((decision, grant)) => {
                            say(format!(
                                "decision {} block={} downgrade={}",
                                decision.method, decision.direct_play_block, decision.downgrade
                            ));
                            let _ = ev.unbounded_send(Ev::Play {
                                url: absolute(&decision.stream_url, &token),
                                hls: decision.is_hls(),
                                method: decision.method.clone(),
                            });
                            _playing = Some(grant);
                        }
                        Err(e) => say(format!("negotiate: {e}")),
                    }
                }
            }
        }
        Ok(())
    }

    /// Every MIME type the spike asks the WebView about; the first block is
    /// `playback.rs`'s table so its `capabilities` can be reused.
    const PROBE_JS: &str = r#"
const mimes = {
  'h264': 'video/mp4; codecs="avc1.640028"',
  'h264 L5.1': 'video/mp4; codecs="avc1.640033"',
  'hevc': 'video/mp4; codecs="hvc1.1.6.L120.90"',
  'hevc main10 L5.1': 'video/mp4; codecs="hvc1.2.4.L153.B0"',
  'hevc hev1 main10': 'video/mp4; codecs="hev1.2.4.L153.B0"',
  'vp9': 'video/mp4; codecs="vp09.00.40.08"',
  'vp9 p2 10-bit': 'video/mp4; codecs="vp09.02.10.10.01.09.16.09.01"',
  'vp9 webm': 'video/webm; codecs="vp9"',
  'av1': 'video/mp4; codecs="av01.0.08M.08"',
  'av1 10-bit': 'video/mp4; codecs="av01.0.08M.10"',
  'dolby vision dvh1.05': 'video/mp4; codecs="dvh1.05.06"',
  'dolby vision dvhe.05': 'video/mp4; codecs="dvhe.05.06"',
  'aac': 'audio/mp4; codecs="mp4a.40.2"',
  'opus': 'audio/mp4; codecs="opus"',
  'flac': 'audio/mp4; codecs="flac"',
  'mp3': 'audio/mpeg',
  'ac3': 'audio/mp4; codecs="ac-3"',
  'eac3': 'audio/mp4; codecs="ec-3"',
  'mp4': 'video/mp4', 'm4v': 'video/mp4', 'mov': 'video/quicktime',
  'webm': 'video/webm', 'mkv': 'video/x-matroska',
  'hls': 'application/vnd.apple.mpegurl',
};
const v = document.createElement('video');
const rows = {};
for (const [name, mime] of Object.entries(mimes)) {
  rows[name] = { mime, mse: !!(window.MediaSource && MediaSource.isTypeSupported(mime)), can: v.canPlayType(mime) };
}
const media = (q) => window.matchMedia(q).matches;
const decode = async (contentType, extra) => {
  try {
    const r = await navigator.mediaCapabilities.decodingInfo({ type: 'media-source',
      video: Object.assign({ contentType, width: 3840, height: 2160, bitrate: 20000000, framerate: 24 }, extra) });
    return `supported=${r.supported} smooth=${r.smooth} powerEfficient=${r.powerEfficient}`;
  } catch (e) { return 'error ' + e; }
};
const hdr = { transferFunction: 'pq', colorGamut: 'rec2020', hdrMetadataType: 'smpteSt2086' };
return {
  rows,
  env: {
    origin: location.origin,
    ua: navigator.userAgent,
    mse: !!window.MediaSource,
    screen: `${screen.width}x${screen.height} dpr ${devicePixelRatio}`,
    'dynamic-range: high': media('(dynamic-range: high)'),
    'video-dynamic-range: high': media('(video-dynamic-range: high)'),
    'color-gamut: rec2020': media('(color-gamut: rec2020)'),
    'decode hevc10 4K PQ': await decode('video/mp4; codecs="hvc1.2.4.L153.B0"', hdr),
    'decode hevc 4K SDR': await decode('video/mp4; codecs="hvc1.1.6.L150.90"', {}),
    'decode av1 10-bit 4K PQ': await decode('video/mp4; codecs="av01.0.12M.10"', hdr),
    'decode vp9 p2 4K PQ': await decode('video/mp4; codecs="vp09.02.51.10.01.09.16.09.01"', hdr),
    'decode dv 4K': await decode('video/mp4; codecs="dvh1.05.06"', hdr),
  },
};
"#;

    /// Key events seen by a plain DOM listener, independent of Dioxus.
    const KEYLOG_JS: &str = r#"
if (!window.__pharosKeys) {
  window.__pharosKeys = true;
  const log = (s) => {
    const el = document.getElementById('jskeys');
    if (el) el.textContent = (s + '\n' + el.textContent).slice(0, 600);
  };
  for (const t of ['keydown', 'keyup']) {
    window.addEventListener(t, (e) => {
      const a = document.activeElement;
      log(`${t} key=${e.key} code=${e.code} keyCode=${e.keyCode} focus=${a ? a.tagName + '.' + a.className : 'none'}`);
    }, true);
  }
  window.addEventListener('popstate', () => log('popstate'));
  history.pushState({ spike: 1 }, '');
  const root = document.querySelector('.root');
  if (root) root.focus();
}
"#;

    #[component]
    fn App() -> Element {
        let mut status = use_signal(Vec::<String>::new);
        let mut code = use_signal(|| None::<String>);
        let mut rail = use_signal(|| None::<(String, Vec<Tile>)>);
        let mut probe = use_signal(|| None::<serde_json::Value>);
        let mut keys = use_signal(Vec::<String>::new);
        let mut playing = use_signal(|| None::<String>);

        use_future(move || async move {
            let rx = EVENTS
                .get()
                .and_then(|m| m.lock().ok().and_then(|mut g| g.take()));
            let Some(mut rx) = rx else { return };
            while let Some(ev) = rx.next().await {
                match ev {
                    Ev::Status(s) => status.write().push(s),
                    Ev::Code(c) => code.set(Some(c)),
                    Ev::Rail(name, tiles) => {
                        code.set(None);
                        rail.set(Some((name, tiles)));
                    }
                    Ev::Play { url, hls, method } => {
                        playing.set(Some(method.clone()));
                        start_video(url, hls).await;
                    }
                }
            }
        });

        use_future(move || async move {
            let _ = document::eval(KEYLOG_JS).await;
            match document::eval(PROBE_JS).await {
                Ok(v) => {
                    log::info!("codec probe {v}");
                    probe.set(Some(v));
                }
                Err(e) => status.write().push(format!("probe: {e}")),
            }
            let hls = document::eval(&format!("{HLS_JS}\nreturn typeof globalThis.Hls + ' ' + (globalThis.Hls && Hls.isSupported());")).await;
            status.write().push(format!("hls.js loaded: {hls:?}"));

            let _ = document::eval("await new Promise(r => setTimeout(r, 3000)); return 1;").await;

            let mut rtt = Vec::with_capacity(200);
            for _ in 0..200 {
                let t0 = std::time::Instant::now();
                let _ = document::eval("return document.querySelectorAll('.tile').length;").await;
                rtt.push(t0.elapsed().as_micros());
            }
            let rtt_min = rtt.iter().copied().min().unwrap_or(0);
            let rtt_max = rtt.iter().copied().max().unwrap_or(0);
            let rtt_p50 = pct(&mut rtt, 0.50);
            let rtt_p95 = pct(&mut rtt, 0.95);
            log::info!(
                "bench rtt (200 evals): min={rtt_min}µs p50={rtt_p50}µs p95={rtt_p95}µs max={rtt_max}µs"
            );

            let mut batch = Vec::with_capacity(50);
            for _ in 0..50 {
                let t0 = std::time::Instant::now();
                let _ = document::eval(
                    r#"return {w: innerWidth, h: innerHeight, sy: scrollY, a: document.activeElement && document.activeElement.className, n: document.querySelectorAll('.tile').length, t: (document.getElementById('v')||{}).currentTime || 0, p: (document.getElementById('v')||{}).paused, vw: (document.getElementById('v')||{}).videoWidth || 0, r: document.querySelector('.root') ? document.querySelector('.root').getBoundingClientRect().width : 0, d: devicePixelRatio};"#,
                )
                .await;
                batch.push(t0.elapsed().as_micros());
            }
            let batch_p50 = pct(&mut batch, 0.50);
            let batch_p95 = pct(&mut batch, 0.95);
            log::info!("bench batched read (50 evals): p50={batch_p50}µs p95={batch_p95}µs");

            let mut e = document::eval(
                r#"for (let i = 0; i < 300; i++) { dioxus.send({i, t: performance.now(), w: innerWidth}); await new Promise(r => requestAnimationFrame(r)); } return 'done';"#,
            );
            let mut gaps = Vec::with_capacity(300);
            let t_start = std::time::Instant::now();
            let mut last = None;
            for _ in 0..300 {
                let _ = e.recv::<serde_json::Value>().await;
                let now = std::time::Instant::now();
                if let Some(prev) = last {
                    gaps.push(now.duration_since(prev).as_micros());
                }
                last = Some(now);
            }
            let total_ms = t_start.elapsed().as_millis();
            let gap_p50 = pct(&mut gaps, 0.50);
            let gap_p95 = pct(&mut gaps, 0.95);
            let gap_max = gaps.iter().copied().max().unwrap_or(0);
            log::info!(
                "bench push throughput (300 frames): gap p50={gap_p50}µs p95={gap_p95}µs max={gap_max}µs total={total_ms}ms"
            );

            let summary = format!(
                "bench: rtt min={rtt_min} p50={rtt_p50} p95={rtt_p95} max={rtt_max}µs | batch p50={batch_p50} p95={batch_p95}µs | push gap p50={gap_p50} p95={gap_p95} max={gap_max}µs ({total_ms}ms)"
            );
            status.write().push(summary);
        });

        let play = move |id: ItemRef| {
            let caps = capabilities(probe.read().as_ref());
            if let Some(tx) = COMMANDS.get() {
                let _ = tx.unbounded_send(Cmd::Play(id, caps));
            }
        };

        rsx! {
            style { {CSS} }
            div {
                class: "root",
                tabindex: "-1",
                onkeydown: move |e| {
                    let line = format!("dioxus keydown key={:?} code={:?}", e.key(), e.code());
                    log::info!("{line}");
                    let mut k = keys.write();
                    k.insert(0, line);
                    k.truncate(8);
                },
                h1 { "pharos TV spike" }
                if let Some(c) = code() {
                    div { class: "code", "QuickConnect code: " b { "{c}" } }
                }
                if let Some((name, tiles)) = rail() {
                    h2 { "{name}" }
                    div { class: "rail",
                        for t in tiles {
                            button {
                                key: "{t.id}",
                                class: "tile",
                                onclick: {
                                    let id = t.id.clone();
                                    move |_| play(id.clone())
                                },
                                if let Some(src) = t.image.clone() {
                                    img { src: "{src}" }
                                }
                                span { "{t.name}" }
                            }
                        }
                    }
                }
                div { class: "player", hidden: playing().is_none(),
                    video { id: "v", muted: true, autoplay: true, controls: true, playsinline: true }
                    if let Some(m) = playing() { p { "method: {m}" } }
                }
                div { class: "cols",
                    pre { class: "log", "status\n" {status.read().join("\n")} }
                    pre { class: "log", "dioxus keys\n" {keys.read().join("\n")} }
                    pre { id: "jskeys", class: "log" }
                }
                if let Some(p) = probe() {
                    pre { class: "log probe", {serde_json::to_string_pretty(&p).unwrap_or_default()} }
                }
            }
        }
    }

    fn pct(v: &mut Vec<u128>, p: f64) -> u128 {
        if v.is_empty() {
            return 0;
        }
        v.sort_unstable();
        let p = if p > 1.0 { p / 100.0 } else { p };
        let idx = ((v.len().saturating_sub(1) as f64) * p).round() as usize;
        v.get(idx).copied().unwrap_or(0)
    }

    fn capabilities(probe: Option<&serde_json::Value>) -> Capabilities {
        let rows = probe.and_then(|p| p.get("rows"));
        let find = |mime: &str| {
            rows.and_then(|r| r.as_object())
                .and_then(|r| r.values().find(|row| row["mime"] == mime))
                .cloned()
        };
        pharos_ui::native::playback::capabilities(
            |mime| {
                find(mime).is_some_and(|r| {
                    r["mse"] == true || r["can"].as_str().is_some_and(|s| !s.is_empty())
                })
            },
            |mime| find(mime).is_some_and(|r| r["can"].as_str().is_some_and(|s| !s.is_empty())),
        )
    }

    async fn start_video(url: String, hls: bool) {
        let url = serde_json::to_string(&url).unwrap_or_default();
        let js = format!(
            r#"
const v = document.getElementById('v');
v.muted = true; v.volume = 0;
const log = (s) => {{ const el = document.getElementById('jskeys'); if (el) el.textContent = s + '\n' + el.textContent; }};
for (const t of ['loadedmetadata','playing','waiting','error','stalled']) v.addEventListener(t, () => log('video ' + t + (v.error ? ' ' + v.error.code + ' ' + v.error.message : '') + ' ' + v.videoWidth + 'x' + v.videoHeight));
// The WebView answers "maybe" for native HLS (its MediaPlayer path); use hls.js.
if ({hls} && Hls.isSupported()) {{
  const h = new Hls();
  h.on(Hls.Events.ERROR, (_, d) => log('hls error ' + d.type + ' ' + d.details + ' fatal=' + d.fatal));
  h.loadSource({url});
  h.attachMedia(v);
}} else {{
  v.src = {url};
}}
try {{ await v.play(); return 'play() ok'; }} catch (e) {{ return 'play() ' + e; }}
"#
        );
        let r = document::eval(&js).await;
        log::info!("start video: {r:?}");
    }

    const CSS: &str = r#"
body { background: #111; color: #eee; font: 18px sans-serif; margin: 24px 48px; }
h1 { font-size: 28px; margin: 0 0 8px; }
.code { font-size: 40px; margin: 16px 0; } .code b { color: #fc6; letter-spacing: 4px; }
.rail { display: flex; gap: 12px; overflow-x: auto; padding: 8px; }
.tile { flex: 0 0 160px; background: #222; color: #eee; border: 3px solid transparent; border-radius: 8px; padding: 4px; }
.tile:focus { border-color: #fc6; outline: none; transform: scale(1.06); }
.tile img { width: 100%; aspect-ratio: 2/3; object-fit: cover; display: block; }
.tile span { font-size: 14px; }
.player video { width: 640px; height: 360px; background: #000; }
.cols { display: flex; gap: 12px; }
.log { flex: 1; font-size: 12px; background: #1b1b1b; padding: 8px; white-space: pre-wrap; max-height: 260px; overflow: auto; }
.probe { max-height: none; }
"#;
}
