//! "Report a problem" (spec 016 US5): what the UI remembers locally so a
//! report can say what led up to a failure, the bundle the user is shown
//! before pressing Send, and the dialog that shows it.
//!
//! Nothing here sends anything. The rings are small and bounded, fed only by
//! route changes, palette and key actions, and the client events the UI
//! raises anyway; they leave the device only inside a report the user sent
//! after reading its exact text. Credentials are cut out before anything
//! enters a ring (FR-013), and the server redacts again.

use std::cell::RefCell;
use std::collections::VecDeque;

use dioxus::prelude::*;
use pharos_client::api::{ClientEvent, ClientEventKind, ClientLevel, ReportSummary};
use serde_json::{json, Value};

/// Most breadcrumbs kept; the oldest is dropped first.
pub const MAX_BREADCRUMBS: usize = 200;
/// Longest breadcrumb text, in bytes.
pub const MAX_CRUMB_BYTES: usize = 256;
/// Most client events kept.
pub const MAX_EVENTS: usize = 50;
/// Most fields kept per event, and the longest value of one.
const MAX_FIELDS: usize = 12;
const MAX_FIELD_BYTES: usize = 256;
/// The server refuses a bundle larger than this (`rpc/report.rs`).
pub const MAX_BUNDLE_BYTES: usize = 256 * 1024;
pub const SCHEMA: &str = "pharos.report/1";
const REDACTED: &str = "<redacted>";

/// Query parameters whose values are credentials or identify a session.
const SECRET_PARAMS: &[&str] = &[
    "api_key",
    "apikey",
    "x-emby-token",
    "token",
    "tag",
    "playsessionid",
];

/// One line of what the user did, with the wall clock it happened at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crumb {
    pub wall_ms: u64,
    pub text: String,
}

/// A client event as kept for a report: labels, not capnp enums.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptEvent {
    pub wall_ms: u64,
    pub kind: &'static str,
    pub level: &'static str,
    pub route: String,
    pub fields: Vec<(String, String)>,
}

/// `text` with the value of every secret `name=value` pair replaced, so a
/// URL's query string or a logged `token=...` cannot carry a credential.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(eq) = rest.find('=') {
        let (head, tail) = rest.split_at(eq);
        let name_start = head
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .map_or(0, |at| at.saturating_add(1));
        let (before, name) = head.split_at(name_start);
        let value = tail.get(1..).unwrap_or("");
        let value_len = value
            .find(|c: char| c == '&' || c == '#' || c == '"' || c == '\'' || c.is_whitespace())
            .unwrap_or(value.len());
        out.push_str(before);
        out.push_str(name);
        out.push('=');
        if SECRET_PARAMS.contains(&name.to_ascii_lowercase().as_str()) {
            out.push_str(REDACTED);
            rest = value.get(value_len..).unwrap_or("");
        } else {
            rest = value;
        }
    }
    out.push_str(rest);
    out
}

/// `text` cut to at most `max` bytes, on a char boundary.
pub fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    text.get(..end).unwrap_or("").to_owned()
}

fn kept_text(text: &str, max: usize) -> String {
    cut(&redact(text), max)
}

/// The label of an event kind in a bundle: the catalogue's own name.
pub fn kind_label(kind: ClientEventKind) -> &'static str {
    match kind {
        ClientEventKind::Panic => "panic",
        ClientEventKind::JsError => "js_error",
        ClientEventKind::RpcFailed => "rpc_failed",
        ClientEventKind::HttpFailed => "http_failed",
        ClientEventKind::WsClosed => "ws_closed",
        ClientEventKind::RendererMode => "renderer_mode",
        ClientEventKind::WebglContextLost => "webgl_context_lost",
        ClientEventKind::LongFrame => "long_frame",
        ClientEventKind::RoutePaint => "route_paint",
        ClientEventKind::SlowInteraction => "slow_interaction",
        ClientEventKind::PlaybackDecision => "playback_decision",
        ClientEventKind::PlaybackStall => "playback_stall",
        ClientEventKind::PlaybackError => "playback_error",
        ClientEventKind::QualitySwitch => "quality_switch",
        ClientEventKind::Seek => "seek",
        ClientEventKind::ThemeFallback => "theme_fallback",
        ClientEventKind::FocusRecovered => "focus_recovered",
        ClientEventKind::CacheServed => "cache_served",
        ClientEventKind::CspViolation => "csp_violation",
        ClientEventKind::GroupDrift => "group_drift",
        ClientEventKind::GroupHardSeek => "group_hard_seek",
        ClientEventKind::GroupWait => "group_wait",
        ClientEventKind::PlaybackResume => "playback_resume",
        ClientEventKind::PlaybackStart => "playback_start",
        ClientEventKind::PlaybackSummary => "playback_summary",
    }
}

pub fn level_label(level: ClientLevel) -> &'static str {
    match level {
        ClientLevel::Error => "error",
        ClientLevel::Warn => "warn",
        ClientLevel::Info => "info",
    }
}

/// The two rings, and the last renderer decision seen (which outlives the
/// event ring's turnover).
#[derive(Debug, Default)]
pub struct Store {
    crumbs: VecDeque<Crumb>,
    events: VecDeque<KeptEvent>,
    renderer: Option<String>,
}

impl Store {
    pub fn note(&mut self, wall_ms: u64, text: &str) {
        if self.crumbs.len() >= MAX_BREADCRUMBS {
            self.crumbs.pop_front();
        }
        self.crumbs.push_back(Crumb {
            wall_ms,
            text: kept_text(text, MAX_CRUMB_BYTES),
        });
    }

    pub fn event(&mut self, wall_ms: u64, event: &ClientEvent) {
        let fields: Vec<(String, String)> = event
            .fields
            .iter()
            .take(MAX_FIELDS)
            .map(|(k, v)| ((*k).to_owned(), kept_text(v, MAX_FIELD_BYTES)))
            .collect();
        if event.kind == ClientEventKind::RendererMode {
            if let Some((_, mode)) = fields.iter().find(|(k, _)| k == "mode") {
                self.renderer = Some(mode.clone());
            }
        }
        if self.events.len() >= MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(KeptEvent {
            wall_ms,
            kind: kind_label(event.kind),
            level: level_label(event.level),
            route: kept_text(&event.route, MAX_FIELD_BYTES),
            fields,
        });
    }

    pub fn clear(&mut self) {
        *self = Store::default();
    }
}

thread_local! {
    static STORE: RefCell<Store> = RefCell::new(Store::default());
}

fn now_ms() -> u64 {
    // Whole milliseconds of a wall clock that is never negative.
    super::platform::wall_clock_ms().max(0.0) as u64
}

/// Whether a key press opens the sheet: D with Ctrl and Shift (the browser
/// reports a capital), not Cmd.
pub fn is_shortcut(key: &str, ctrl: bool, meta: bool) -> bool {
    ctrl && !meta && key == "D"
}

/// Appends a breadcrumb: a route change, a palette or key action.
pub fn note(text: &str) {
    STORE.with(|s| s.borrow_mut().note(now_ms(), text));
}

/// Keeps `event` for a report, whether or not diagnostics are opted in.
pub fn record_event(event: &ClientEvent) {
    STORE.with(|s| s.borrow_mut().event(now_ms(), event));
}

/// Forgets both rings, at sign-out: the next user's report must not carry
/// the last one's trail.
pub fn forget() {
    STORE.with(|s| s.borrow_mut().clear());
}

/// Everything a bundle is built from, gathered once when the sheet opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportContext {
    pub version: String,
    pub build: String,
    pub user_agent: String,
    pub renderer: String,
    pub route: String,
    pub breadcrumbs: Vec<Crumb>,
    pub events: Vec<KeptEvent>,
    pub captured_at_ms: u64,
}

impl ReportContext {
    /// The rings and the page as they are now.
    pub fn capture() -> Self {
        let (breadcrumbs, events, renderer) = STORE.with(|s| {
            let s = s.borrow();
            (
                s.crumbs.iter().cloned().collect(),
                s.events.iter().cloned().collect(),
                s.renderer.clone(),
            )
        });
        ReportContext {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            build: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
            .to_owned(),
            user_agent: super::platform::user_agent(),
            renderer: renderer.unwrap_or_else(|| "unknown".to_owned()),
            route: kept_text(&super::platform::current_path(), MAX_FIELD_BYTES),
            breadcrumbs,
            events,
            captured_at_ms: now_ms(),
        }
    }
}

/// The report's JSON object. Every free-text value is redacted again here,
/// so a context built by hand cannot carry a credential either.
pub fn build_bundle(ctx: &ReportContext) -> Value {
    let breadcrumbs: Vec<Value> = ctx
        .breadcrumbs
        .iter()
        .map(|c| json!({"wallMs": c.wall_ms, "text": kept_text(&c.text, MAX_CRUMB_BYTES)}))
        .collect();
    let events: Vec<Value> = ctx
        .events
        .iter()
        .map(|e| {
            let fields: serde_json::Map<String, Value> = e
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), Value::from(kept_text(v, MAX_FIELD_BYTES))))
                .collect();
            json!({
                "wallMs": e.wall_ms,
                "kind": e.kind,
                "level": e.level,
                "route": kept_text(&e.route, MAX_FIELD_BYTES),
                "fields": fields,
            })
        })
        .collect();
    json!({
        "schema": SCHEMA,
        "app": {"version": ctx.version, "build": ctx.build},
        "browser": {"userAgent": ctx.user_agent, "renderer": ctx.renderer},
        "route": kept_text(&ctx.route, MAX_FIELD_BYTES),
        "breadcrumbs": breadcrumbs,
        "events": events,
        "traceIds": [],
        "capturedAtMs": ctx.captured_at_ms,
    })
}

/// The exact text the user is shown and, on Send, the exact bytes sent. If
/// it would pass the server's cap the older half of both rings is dropped
/// until it fits (escaping can make a few long values larger than they look).
pub fn render(ctx: &ReportContext) -> String {
    let mut ctx = ctx.clone();
    loop {
        let text = serde_json::to_string_pretty(&build_bundle(&ctx)).unwrap_or_default();
        if text.len() <= MAX_BUNDLE_BYTES || (ctx.breadcrumbs.is_empty() && ctx.events.is_empty()) {
            return text;
        }
        ctx.breadcrumbs.drain(..ctx.breadcrumbs.len() / 2);
        ctx.events.drain(..ctx.events.len() / 2);
    }
}

/// `2026-11-09 14:03 UTC` for a wall clock in milliseconds.
pub fn utc_label(wall_ms: u64) -> String {
    let secs = wall_ms / 1000;
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (Hinnant's algorithm).
    let z = days.saturating_add(719_468);
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rem / 3_600,
        rem % 3_600 / 60
    )
}

/// The "Your problem reports" rows, one per stored report.
#[component]
pub fn ReportList(reports: Vec<ReportSummary>, on_delete: EventHandler<String>) -> Element {
    rsx! {
        if reports.is_empty() {
            p { class: "pharos-setting-help", "You have not sent any problem reports." }
        } else {
            ul { class: "pharos-report-list",
                for report in reports {
                    li { key: "{report.id}", class: "pharos-report-row",
                        div { class: "pharos-setting-text",
                            span { class: "pharos-setting-name",
                                "{report.id} · {utc_label(report.received_wall_ms)}"
                            }
                            if !report.note.is_empty() {
                                span { class: "pharos-setting-help", "{report.note}" }
                            }
                        }
                        button {
                            class: "pharos-button pharos-button-secondary",
                            r#type: "button",
                            "aria-label": "Delete report {report.id}",
                            onclick: {
                                let id = report.id.clone();
                                move |_| on_delete.call(id.clone())
                            },
                            "Delete"
                        }
                    }
                }
            }
        }
    }
}

pub const NOTE_ID: &str = "pharos-report-note";
pub const SEND_ID: &str = "pharos-report-send";

/// The sheet: a note, the exact text that will be sent, Send and Cancel;
/// after a send, the report's id or the error. Pure: the app owns the state.
#[component]
pub fn ReportView(
    open: bool,
    note: String,
    preview: String,
    sending: bool,
    result: Option<Result<String, String>>,
    on_note: EventHandler<String>,
    on_send: EventHandler<()>,
    on_close: EventHandler<()>,
    on_copy: EventHandler<String>,
) -> Element {
    if !open {
        return rsx! {};
    }
    rsx! {
        ReportDialog { note, preview, sending, result, on_note, on_send, on_close, on_copy }
    }
}

#[component]
fn ReportDialog(
    note: String,
    preview: String,
    sending: bool,
    result: Option<Result<String, String>>,
    on_note: EventHandler<String>,
    on_send: EventHandler<()>,
    on_close: EventHandler<()>,
    on_copy: EventHandler<String>,
) -> Element {
    super::views::use_modal_hold(".pharos-report-backdrop");
    let sent = match &result {
        Some(Ok(id)) => Some(id.clone()),
        _ => None,
    };
    rsx! {
        div { class: "pharos-whats-new-backdrop pharos-report-backdrop",
            div {
                class: "pharos-whats-new pharos-report",
                role: "dialog",
                aria_modal: "true",
                aria_labelledby: "pharos-report-title",
                tabindex: "-1",
                onkeydown: move |e: KeyboardEvent| {
                    e.stop_propagation();
                    if e.key() == Key::Named(NamedKey::Escape) {
                        on_close.call(());
                    }
                },
                onclick: move |e: MouseEvent| e.stop_propagation(),
                header { class: "pharos-whats-new-head",
                    h2 { id: "pharos-report-title", "Report a problem" }
                }
                if let Some(id) = sent {
                    p { class: "pharos-report-sent", role: "status",
                        "Report sent. Its id is "
                        strong { class: "pharos-report-id", "{id}" }
                        "."
                    }
                    p { class: "pharos-whats-new-help",
                        "Quote this id when you tell the server's admin what went wrong"
                    }
                    footer { class: "pharos-whats-new-actions",
                        button {
                            class: "pharos-button pharos-button-secondary",
                            r#type: "button",
                            onclick: {
                                let id = id.clone();
                                move |_| on_copy.call(id.clone())
                            },
                            "Copy"
                        }
                        button {
                            id: SEND_ID,
                            class: "pharos-button",
                            r#type: "button",
                            onclick: move |_| on_close.call(()),
                            "Close"
                        }
                    }
                } else {
                    p { class: "pharos-whats-new-help",
                        "Nothing is sent until you press Send. This goes only to this server."
                    }
                    label { class: "pharos-report-note",
                        span { class: "pharos-setting-name", "What went wrong? (optional)" }
                        textarea {
                            id: NOTE_ID,
                            rows: "4",
                            value: "{note}",
                            oninput: move |e: FormEvent| on_note.call(e.value()),
                        }
                    }
                    details { class: "pharos-report-details",
                        summary { "What will be sent" }
                        pre { class: "pharos-report-preview", "{preview}" }
                    }
                    if let Some(Err(message)) = &result {
                        p { class: "pharos-error", role: "alert", "Could not send the report: {message}" }
                    }
                    footer { class: "pharos-whats-new-actions",
                        button {
                            class: "pharos-button pharos-button-secondary",
                            r#type: "button",
                            onclick: move |_| on_close.call(()),
                            "Cancel"
                        }
                        button {
                            id: SEND_ID,
                            class: "pharos-button",
                            r#type: "button",
                            disabled: sending,
                            onclick: move |_| on_send.call(()),
                            if sending { "Sending…" } else { "Send" }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(
        kind: ClientEventKind,
        route: &str,
        fields: Vec<(&'static str, String)>,
    ) -> ClientEvent {
        ClientEvent {
            kind,
            level: ClientLevel::Warn,
            route: route.to_owned(),
            fields,
        }
    }

    fn context(store: &Store) -> ReportContext {
        ReportContext {
            version: "1.2.3".into(),
            build: "release".into(),
            user_agent: "UA/1".into(),
            renderer: store.renderer.clone().unwrap_or_else(|| "unknown".into()),
            route: "/watch/9".into(),
            breadcrumbs: store.crumbs.iter().cloned().collect(),
            events: store.events.iter().cloned().collect(),
            captured_at_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn secret_params_are_redacted_whatever_their_case() {
        let url = "/Videos/1/master.m3u8?api_key=SECRET1&Foo=ok&X-Emby-Token=SECRET2\
                   &TOKEN=SECRET3&tag=SECRET4&PlaySessionId=SECRET5#frag";
        let out = redact(url);
        for secret in ["SECRET1", "SECRET2", "SECRET3", "SECRET4", "SECRET5"] {
            assert!(!out.contains(secret), "{secret} survived: {out}");
        }
        assert!(out.contains("Foo=ok"), "{out}");
        assert!(out.ends_with("#frag"), "{out}");
    }

    #[test]
    fn apikey_spelled_either_way_is_redacted() {
        assert_eq!(redact("x?ApiKey=abc&y=1"), "x?ApiKey=<redacted>&y=1");
        assert_eq!(redact("x?api_key=abc"), "x?api_key=<redacted>");
    }

    #[test]
    fn text_without_secrets_is_unchanged() {
        assert_eq!(redact("palette: Go to Home"), "palette: Go to Home");
        assert_eq!(redact("a=b=c"), "a=b=c");
        assert_eq!(redact("tokens=7"), "tokens=7");
    }

    #[test]
    fn a_crumb_is_cut_on_a_char_boundary() {
        let mut store = Store::default();
        store.note(1, &"é".repeat(300));
        let text = &store.crumbs[0].text;
        assert!(text.len() <= MAX_CRUMB_BYTES, "{}", text.len());
        assert_eq!(text.len(), 256, "two-byte chars fit exactly");
        let mut store = Store::default();
        store.note(1, &format!("a{}", "€".repeat(100)));
        assert!(store.crumbs[0].text.len() <= MAX_CRUMB_BYTES);
        assert!(store.crumbs[0].text.chars().all(|c| c == 'a' || c == '€'));
    }

    #[test]
    fn the_rings_keep_only_the_newest() {
        let mut store = Store::default();
        for n in 0..(MAX_BREADCRUMBS as u64 + 5) {
            store.note(n, &format!("c{n}"));
        }
        assert_eq!(store.crumbs.len(), MAX_BREADCRUMBS);
        assert_eq!(store.crumbs.front().map(|c| c.wall_ms), Some(5));
        for n in 0..(MAX_EVENTS + 3) {
            store.event(
                n as u64,
                &event(ClientEventKind::Seek, "/", vec![("n", n.to_string())]),
            );
        }
        assert_eq!(store.events.len(), MAX_EVENTS);
        assert_eq!(store.events.front().map(|e| e.wall_ms), Some(3));
    }

    #[test]
    fn a_bundle_carries_no_secret_from_a_crumb_or_an_event_field() {
        let mut store = Store::default();
        store.note(1, "open /Videos/1/stream?api_key=CRUMBSECRET&x=1");
        store.event(
            2,
            &event(
                ClientEventKind::PlaybackError,
                "/watch/1?token=ROUTESECRET",
                vec![(
                    "url",
                    "https://h/seg?X-Emby-Token=FIELDSECRET&PlaySessionId=PS".into(),
                )],
            ),
        );
        let mut ctx = context(&store);
        // A context built by hand is redacted again at the bundle.
        ctx.breadcrumbs.push(Crumb {
            wall_ms: 3,
            text: "raw tag=RAWSECRET".into(),
        });
        let text = render(&ctx);
        for secret in [
            "CRUMBSECRET",
            "ROUTESECRET",
            "FIELDSECRET",
            "PS\"",
            "RAWSECRET",
        ] {
            assert!(!text.contains(secret), "{secret} survived:\n{text}");
        }
        assert!(text.contains("<redacted>"), "{text}");
    }

    #[test]
    fn the_bundle_has_the_documented_shape() {
        let mut store = Store::default();
        store.event(
            5,
            &event(
                ClientEventKind::RendererMode,
                "/",
                vec![("mode", "fallback".into())],
            ),
        );
        let bundle = build_bundle(&context(&store));
        assert_eq!(bundle["schema"], SCHEMA);
        assert_eq!(bundle["app"]["version"], "1.2.3");
        assert_eq!(bundle["browser"]["userAgent"], "UA/1");
        assert_eq!(bundle["browser"]["renderer"], "fallback");
        assert_eq!(bundle["route"], "/watch/9");
        assert_eq!(bundle["traceIds"], json!([]));
        assert_eq!(bundle["capturedAtMs"], 1_700_000_000_000_u64);
        assert_eq!(bundle["events"][0]["kind"], "renderer_mode");
        assert_eq!(bundle["events"][0]["level"], "warn");
        assert_eq!(bundle["events"][0]["fields"]["mode"], "fallback");
    }

    #[test]
    fn a_full_store_still_fits_the_servers_cap() {
        let mut store = Store::default();
        for n in 0..MAX_BREADCRUMBS {
            store.note(n as u64, &"\u{1}".repeat(300));
        }
        for n in 0..MAX_EVENTS {
            let fields = (0..MAX_FIELDS)
                .map(|_| ("k", "\u{1}".repeat(300)))
                .collect();
            store.event(n as u64, &event(ClientEventKind::Seek, "/", fields));
        }
        let text = render(&context(&store));
        assert!(text.len() <= MAX_BUNDLE_BYTES, "{}", text.len());
    }

    #[test]
    fn every_event_kind_has_a_distinct_label() {
        let kinds = [
            ClientEventKind::Panic,
            ClientEventKind::JsError,
            ClientEventKind::RpcFailed,
            ClientEventKind::HttpFailed,
            ClientEventKind::WsClosed,
            ClientEventKind::RendererMode,
            ClientEventKind::WebglContextLost,
            ClientEventKind::LongFrame,
            ClientEventKind::RoutePaint,
            ClientEventKind::SlowInteraction,
            ClientEventKind::PlaybackDecision,
            ClientEventKind::PlaybackStall,
            ClientEventKind::PlaybackError,
            ClientEventKind::QualitySwitch,
            ClientEventKind::Seek,
            ClientEventKind::ThemeFallback,
            ClientEventKind::FocusRecovered,
            ClientEventKind::CacheServed,
            ClientEventKind::CspViolation,
            ClientEventKind::GroupDrift,
            ClientEventKind::GroupHardSeek,
            ClientEventKind::GroupWait,
            ClientEventKind::PlaybackResume,
            ClientEventKind::PlaybackStart,
            ClientEventKind::PlaybackSummary,
        ];
        let mut labels: Vec<_> = kinds.iter().map(|k| kind_label(*k)).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), kinds.len());
    }

    #[test]
    fn ctrl_shift_d_opens_the_sheet_and_nothing_else_does() {
        assert!(is_shortcut("D", true, false));
        assert!(!is_shortcut("d", true, false));
        assert!(!is_shortcut("D", false, false));
        assert!(!is_shortcut("D", true, true));
    }

    #[test]
    fn utc_labels_are_calendar_dates() {
        assert_eq!(utc_label(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc_label(1_709_208_300_000), "2024-02-29 12:05 UTC");
        assert_eq!(utc_label(1_700_000_000_000), "2023-11-14 22:13 UTC");
    }

    fn sheet(result: Option<Result<String, String>>) -> String {
        let mut dom = VirtualDom::new_with_props(
            |result: Option<Result<String, String>>| {
                rsx! {
                    ReportView {
                        open: true,
                        note: "it froze".to_owned(),
                        preview: "{\n  \"schema\": \"pharos.report/1\"\n}".to_owned(),
                        sending: false,
                        result,
                        on_note: |_| {},
                        on_send: |_| {},
                        on_close: |_| {},
                        on_copy: |_| {},
                    }
                }
            },
            result,
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn the_sheet_shows_exactly_what_will_be_sent_before_sending() {
        let html = sheet(None);
        assert!(html.contains("role=\"dialog\""), "{html}");
        assert!(html.contains("aria-modal=\"true\""), "{html}");
        assert!(html.contains("Report a problem"), "{html}");
        assert!(html.contains("What will be sent"), "{html}");
        assert!(html.contains("pharos.report/1"), "{html}");
        assert!(html.contains("it froze"), "{html}");
        assert!(html.contains(">Send<"), "{html}");
        assert!(html.contains(">Cancel<"), "{html}");
    }

    #[test]
    fn the_sheet_shows_the_id_after_a_send_and_the_error_after_a_failure() {
        let sent = sheet(Some(Ok("K7QM-2XRT".to_owned())));
        assert!(sent.contains("K7QM-2XRT"), "{sent}");
        assert!(sent.contains(">Copy<"), "{sent}");
        assert!(
            sent.contains("Quote this id when you tell the server&#39;s admin what went wrong")
                || sent.contains("Quote this id when you tell the server's admin what went wrong"),
            "{sent}"
        );
        let failed = sheet(Some(Err("server said no".to_owned())));
        assert!(failed.contains("server said no"), "{failed}");
        assert!(
            failed.contains(">Send<"),
            "a failed send can be retried: {failed}"
        );
    }

    #[test]
    fn a_closed_sheet_renders_nothing() {
        let mut dom = VirtualDom::new(|| {
            rsx! {
                ReportView {
                    open: false,
                    note: String::new(),
                    preview: String::new(),
                    sending: false,
                    result: None,
                    on_note: |_| {},
                    on_send: |_| {},
                    on_close: |_| {},
                    on_copy: |_| {},
                }
            }
        });
        dom.rebuild_in_place();
        assert_eq!(dioxus_ssr::render(&dom), "");
    }
}
