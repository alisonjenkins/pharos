//! The native app: routing and history, the connection lifecycle that
//! drives [`Machine`], and the screens that load data for the pure views.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use dioxus::prelude::*;
use futures_util::StreamExt as _;
use pharos_client::api::{
    Detail, Home, ItemKind, ItemRef, NativeApi, NativeGrant, PharosApi as _, PlaybackGrant as _,
    TrackChoice, TrackPicks,
};
use pharos_client::{CallInfo, ClientError};

use super::appearance::{AppearanceView, ContrastRow, Edit, EditorView, Notice, ThemeCard};
use super::backdrop::Backdrop;
use super::browse::{self, Neighbours};
use super::diagnostics;
use super::find::{Find, DEBOUNCE_MS, RESULT_LIMIT};
use super::listen::ListenView;
use super::machine::{Event, Machine, Phase};
use super::platform::{self, OpenError};
use super::playback::{
    choose_engine, position_at, preview_set, start_ms, subtitle_switch, Engine, MediaEvent,
    MediaState, OnFatal, Outgoing, PlaybackEngine, ReportClock, SubtitleSwitch,
};
use super::route::Route;
use super::views::{
    ErrorNote, FindView, Hits, HomeView, Loading, Nav, Shell, SignInView, StatusView, TitleView,
};
use super::watch_view::{
    text_track_id, Control, Controls, WatchView, PLAYER_ID, SEEK_ID, VIDEO_ID,
};
use crate::theme::editor::{Draft, EffectField};
use crate::theme::resolve::PaletteRole;
use crate::theme::signal::{self, Source};
use crate::theme::value::{Density, FontStack, Rgba};
use crate::theme::{self, contrast, css, device, doc, format, Catalogue, Theme, ThemeError};
use crate::views::RootApp;

/// The page root: one route per URL, kept in step with history. `/manage`
/// mounts the older REST panes and never opens the native socket.
#[component]
pub fn Root() -> Element {
    let boot = use_hook(boot_theme);
    let themes = Themes {
        catalogue: use_signal(|| boot.catalogue.clone()),
        active: use_signal(|| boot.active.clone()),
        applied: use_signal(|| boot.applied.clone()),
    };
    use_context_provider(|| themes);
    use_hook(move || Rc::new(watch_system_preferences(themes)));
    let mut route = use_signal(|| Route::parse(&platform::current_path()));
    use_hook(|| {
        Rc::new(platform::on_pop_state(move |path| {
            route.set(Route::parse(&path))
        }))
    });
    use_context_provider(|| {
        Nav(EventHandler::new(move |to: Route| {
            if *route.peek() != to {
                platform::push(&to.path());
                route.set(to);
            }
        }))
    });
    use_context_provider(|| {
        Replace(EventHandler::new(move |to: Route| {
            platform::replace(&to.path());
            route.set(to);
        }))
    });
    match route() {
        Route::Manage => rsx! { RootApp {} },
        route => rsx! {
            Backdrop {}
            NativeApp { route }
        },
    }
}

const PREFERS_DARK: &str = "(prefers-color-scheme: dark)";
const PREFERS_MORE_CONTRAST: &str = "(prefers-contrast: more)";
const IMPORT_ACCEPT: &str = ".toml,application/toml";
const THEME_MIME: &str = "application/toml";
/// The colour shown for a role when neither the draft nor its base resolves.
const UNRESOLVED: Rgba = Rgba {
    r: 0,
    g: 0,
    b: 0,
    a: 255,
};

/// The theme state every screen shares: the named themes, the choice (a
/// name or `auto`) and the name applied now.
#[derive(Clone, Copy)]
pub struct Themes {
    catalogue: Signal<Catalogue>,
    active: Signal<String>,
    applied: Signal<String>,
}

impl Themes {
    /// The theme applied now, resolved from the catalogue; reading it
    /// subscribes the caller to theme changes.
    pub fn applied_theme(&self) -> Option<Theme> {
        let name = self.applied.read();
        self.catalogue.read().resolve(&name).ok().map(|r| r.theme)
    }

    /// Apply the active choice and record the name applied. `auto` resolves
    /// against the system's preferences at this moment.
    fn apply_active(mut self) {
        let active = self.active.peek().clone();
        let (dark, more) = prefers();
        let catalogue = self.catalogue.peek();
        match theme::device::startup(&catalogue, Ok(Some(active)), dark, more) {
            Ok(startup) => {
                log_lines(&startup.lines);
                apply_theme(&startup.theme);
                self.applied.set(startup.theme.name);
            }
            Err(e) => warn_unavailable(&e),
        }
    }

    /// Make `name` the choice, apply it and store the choice.
    fn choose(mut self, name: String) {
        self.active.set(name);
        self.apply_active();
        self.store_active();
    }

    fn store_active(self) {
        store_setting(device::ACTIVE_KEY, &self.active.peek());
    }

    fn store_custom(self) {
        let json = device::custom_json(&self.catalogue.peek());
        store_setting(device::CUSTOM_KEY, &json);
    }

    /// The catalogue entry `name` changed: re-apply it when it is the active
    /// choice, and store the custom themes.
    fn catalogue_changed(self, name: &str) {
        if *self.active.peek() == name {
            self.apply_active();
        }
        self.store_custom();
    }

    /// The system's preferences changed: re-resolve when `auto` is chosen.
    fn follow_system(self) {
        if *self.active.peek() == device::AUTO {
            self.apply_active();
        }
    }
}

/// The device's catalogue and choices at start-up.
#[derive(Clone)]
struct Boot {
    catalogue: Catalogue,
    active: String,
    applied: String,
}

/// Load the custom themes, then apply the device's stored choice before the
/// first native paint, logging which theme and, on a fallback, why.
fn boot_theme() -> Boot {
    let mut catalogue = theme::shipped();
    log_lines(&device::load_custom(
        &mut catalogue,
        platform::stored_theme(device::CUSTOM_KEY),
    ));
    let stored = platform::stored_theme(device::ACTIVE_KEY);
    let auto = matches!(&stored, Ok(Some(name)) if name == device::AUTO);
    let (dark, more) = prefers();
    match device::startup(&catalogue, stored, dark, more) {
        Ok(startup) => {
            log_lines(&startup.lines);
            apply_theme(&startup.theme);
            let active = if auto {
                device::AUTO.to_owned()
            } else {
                startup.theme.name.clone()
            };
            Boot {
                catalogue,
                active,
                applied: startup.theme.name,
            }
        }
        Err(e) => {
            warn_unavailable(&e);
            Boot {
                catalogue,
                active: theme::DEFAULT.to_owned(),
                applied: theme::DEFAULT.to_owned(),
            }
        }
    }
}

/// Keep the two system-preference listeners for the page's life; a change
/// re-resolves `auto`.
fn watch_system_preferences(themes: Themes) -> [platform::MediaWatch; 2] {
    [
        platform::on_media_change(PREFERS_DARK, move || themes.follow_system()),
        platform::on_media_change(PREFERS_MORE_CONTRAST, move || themes.follow_system()),
    ]
}

fn prefers() -> (bool, bool) {
    (
        platform::prefers(PREFERS_DARK),
        platform::prefers(PREFERS_MORE_CONTRAST),
    )
}

/// Set the page's tokens in one style write. A failure is logged and the
/// page keeps the tokens it had.
fn apply_theme(theme: &Theme) {
    if let Err(e) = platform::apply_tokens(&css::declarations(theme)) {
        platform::warn(&format!(
            "pharos: theme apply failed name={} error={e}",
            theme.name
        ));
    }
}

fn warn_unavailable(e: &ThemeError) {
    platform::warn(&format!(
        "pharos: theme unavailable reason={} keeping=pharos.css",
        e.label()
    ));
}

fn log_lines(lines: &[device::Line]) {
    for line in lines {
        match line {
            device::Line::Info(l) => platform::info(l),
            device::Line::Warn(l) => {
                platform::warn(l);
                if l.starts_with("pharos: theme fallback") {
                    platform::report_event(pharos_client::api::ClientEvent {
                        kind: pharos_client::api::ClientEventKind::ThemeFallback,
                        level: pharos_client::api::ClientLevel::Warn,
                        route: String::new(),
                        fields: vec![("line", l.clone())],
                    });
                }
            }
        }
    }
}

fn store_setting(key: &str, value: &str) {
    if let Err(e) = platform::store_theme(key, value) {
        platform::warn(&signal::storage_failed(key, &e));
    }
}

fn source_label(name: &str) -> Source {
    if theme::is_shipped(name) {
        Source::Shipped
    } else {
        Source::Local
    }
}

fn unknown_theme(name: &str) {
    platform::warn(&signal::fallback(
        name,
        source_label(name),
        &ThemeError::UnknownTheme {
            name: name.to_owned(),
        },
        "-",
    ));
}

fn bad_field(path: &str, value: &str) -> ThemeError {
    ThemeError::BadValue {
        path: path.to_owned(),
        value: value.to_owned(),
        expected: "a value the editor offers".to_owned(),
    }
}

fn notify(mut notice: Signal<Option<Notice>>, text: String, error: bool) {
    notice.set(Some(Notice { text, error }));
}

/// Store `source` as a custom theme. The outcome's lines are logged; the
/// saved name, or the reason for refusing, comes back.
fn save_theme(mut themes: Themes, source: String, from: Source) -> Result<String, String> {
    let outcome = {
        let mut catalogue = themes.catalogue.write();
        device::save(&mut catalogue, source, from)
    };
    match outcome {
        Ok(saved) => {
            log_lines(&saved.lines);
            Ok(saved.name)
        }
        Err(refusal) => {
            log_lines(&refusal.lines);
            Err(refusal.reason)
        }
    }
}

fn import(themes: Themes, notice: Signal<Option<Notice>>, source: String) {
    match save_theme(themes, source, Source::Import) {
        Ok(name) => {
            themes.catalogue_changed(&name);
            notify(notice, format!("Imported {name}"), false);
        }
        Err(reason) => notify(notice, reason, true),
    }
}

fn import_file(themes: Themes, notice: Signal<Option<Notice>>) {
    spawn(async move {
        match platform::pick_file(IMPORT_ACCEPT, doc::MAX_BYTES).await {
            Ok(Some(text)) => import(themes, notice, text),
            Ok(None) => {}
            Err(e) => {
                platform::warn(&signal::fallback("-", Source::Import, &e, "-"));
                notify(notice, e.to_string(), true);
            }
        }
    });
}

fn export(themes: Themes, notice: Signal<Option<Notice>>, name: &str) {
    let source = themes.catalogue.peek().get_source(name).map(str::to_owned);
    let Some(source) = source else {
        unknown_theme(name);
        return;
    };
    let filename = format!("{}.pharos-theme.toml", theme::slug(name));
    match platform::download(&filename, THEME_MIME, &source) {
        Ok(()) => platform::info(&signal::exported(name, source.len())),
        Err(e) => {
            platform::warn(&signal::fallback(name, source_label(name), &e, "-"));
            notify(notice, e.to_string(), true);
        }
    }
}

fn delete(
    mut themes: Themes,
    mut editing: Signal<Option<Editing>>,
    notice: Signal<Option<Notice>>,
    name: &str,
) {
    if theme::is_shipped(name) {
        platform::warn(&signal::fallback(
            name,
            Source::Shipped,
            &ThemeError::BadValue {
                path: "meta.name".to_owned(),
                value: name.to_owned(),
                expected: "a custom theme".to_owned(),
            },
            "-",
        ));
        return;
    }
    if !themes.catalogue.write().remove(name) {
        unknown_theme(name);
        return;
    }
    platform::info(&signal::deleted(name));
    let editing_it = editing
        .peek()
        .as_ref()
        .is_some_and(|state| state.base == name || state.draft.name() == Some(name));
    if editing_it {
        editing.set(None);
    }
    if *themes.active.peek() == name {
        themes.choose(theme::DEFAULT.to_owned());
    } else if editing_it {
        themes.apply_active();
    }
    themes.store_custom();
    notify(notice, format!("Deleted {name}"), false);
}

fn customise(
    themes: Themes,
    mut editing: Signal<Option<Editing>>,
    mut notice: Signal<Option<Notice>>,
    base: String,
) {
    let name = fresh_name(&themes.catalogue.peek(), &base);
    let draft = Draft::new(&name, &base);
    apply_preview(themes, &draft);
    editing.set(Some(Editing { draft, base }));
    notice.set(None);
}

fn edit_custom(
    themes: Themes,
    mut editing: Signal<Option<Editing>>,
    mut notice: Signal<Option<Notice>>,
    name: &str,
) {
    let source = themes.catalogue.peek().get_source(name).map(str::to_owned);
    let Some(source) = source else {
        unknown_theme(name);
        return;
    };
    match Draft::from_source(&source) {
        Ok(draft) => {
            let base = format::read(&source)
                .ok()
                .and_then(|(doc, _)| doc.extends().map(str::to_owned))
                .unwrap_or_else(|| theme::DEFAULT.to_owned());
            apply_preview(themes, &draft);
            editing.set(Some(Editing { draft, base }));
            notice.set(None);
        }
        Err(e) => {
            platform::warn(&signal::fallback(name, source_label(name), &e, "-"));
            notify(notice, e.to_string(), true);
        }
    }
}

/// Save the draft as a custom theme and make it the choice. A refusal keeps
/// the editor open with its reason.
fn save_editor(
    themes: Themes,
    mut editing: Signal<Option<Editing>>,
    notice: Signal<Option<Notice>>,
) {
    let Some(state) = editing.peek().clone() else {
        return;
    };
    let source = match state.draft.source() {
        Ok(source) => source,
        Err(e) => {
            platform::warn(&signal::fallback(
                state.draft.name().unwrap_or("-"),
                Source::Editor,
                &e,
                "-",
            ));
            notify(notice, e.to_string(), true);
            return;
        }
    };
    match save_theme(themes, source, Source::Editor) {
        Ok(name) => {
            editing.set(None);
            themes.choose(name.clone());
            themes.store_custom();
            notify(notice, format!("Saved {name}"), false);
        }
        Err(reason) => notify(notice, reason, true),
    }
}

/// Apply one editor change to the draft, then preview it live. A change the
/// editor could not have sent is logged and dropped.
fn edit_draft(themes: Themes, mut editing: Signal<Option<Editing>>, edit: Edit) {
    let Some(mut state) = editing.peek().clone() else {
        return;
    };
    match apply_edit(&mut state.draft, edit) {
        Ok(()) => {
            apply_preview(themes, &state.draft);
            editing.set(Some(state));
        }
        Err(e) => platform::warn(&signal::fallback(
            state.draft.name().unwrap_or("-"),
            Source::Editor,
            &e,
            "-",
        )),
    }
}

fn apply_edit(draft: &mut Draft, edit: Edit) -> Result<(), ThemeError> {
    match edit {
        Edit::Name(name) => draft.set_name(&name),
        Edit::Colour(role, hex) => {
            let parsed = PaletteRole::parse(&role).ok_or_else(|| bad_field("palette", &role))?;
            draft.set_colour(parsed, &hex);
        }
        Edit::Density(label) => {
            let density =
                Density::parse(&label).ok_or_else(|| bad_field("shape.density", &label))?;
            draft.set_density(density);
        }
        Edit::UiFont(label) => {
            let font = FontStack::parse(&label).ok_or_else(|| bad_field("type.ui", &label))?;
            draft.set_ui_font(font);
        }
        Edit::Effect(label, value) => {
            let field = effect_field(&label).ok_or_else(|| bad_field("effects", &label))?;
            draft.set_effect(field, value);
        }
    }
    Ok(())
}

fn effect_field(label: &str) -> Option<EffectField> {
    match label {
        "blur" => Some(EffectField::Blur),
        "vignette" => Some(EffectField::Vignette),
        "grain" => Some(EffectField::Grain),
        "glow" => Some(EffectField::Glow),
        _ => None,
    }
}

/// Show the draft's resolved tokens without saving them. A draft that does
/// not resolve keeps the tokens already applied, and logs why.
fn apply_preview(themes: Themes, draft: &Draft) {
    match draft.preview(&themes.catalogue.peek()) {
        Ok(resolved) => apply_theme(&resolved.theme),
        Err(e) => platform::warn(&signal::fallback(
            draft.name().unwrap_or("-"),
            Source::Editor,
            &e,
            &themes.applied.peek(),
        )),
    }
}

fn cancel(
    themes: Themes,
    mut editing: Signal<Option<Editing>>,
    mut notice: Signal<Option<Notice>>,
) {
    editing.set(None);
    notice.set(None);
    themes.apply_active();
}

/// `My <base>`, or the first `My <base> <n>` no custom theme already uses.
fn fresh_name(catalogue: &Catalogue, base: &str) -> String {
    let first = format!("My {base}");
    if catalogue.get_source(&first).is_none() {
        return first;
    }
    (2..)
        .map(|n| format!("My {base} {n}"))
        .find(|name| catalogue.get_source(name).is_none())
        .unwrap_or(first)
}

/// The theme cards for the catalogue. A theme that does not resolve gets no
/// card; its fallback line is logged instead.
fn theme_cards(catalogue: &Catalogue) -> Vec<ThemeCard> {
    let mut cards = Vec::new();
    for name in catalogue.names() {
        match catalogue.resolve(name) {
            Ok(resolved) => cards.push(card(name, &resolved.theme)),
            Err(e) => platform::warn(&signal::fallback(name, source_label(name), &e, "-")),
        }
    }
    cards
}

fn card(name: &str, theme: &Theme) -> ThemeCard {
    let p = &theme.palette;
    ThemeCard {
        name: name.to_owned(),
        shipped: theme::is_shipped(name),
        active: false,
        swatches: [p.bg, p.surface, p.accent, p.fg]
            .iter()
            .map(Rgba::to_css)
            .collect(),
    }
}

/// `#rrggbb`, the only form an `<input type=color>` accepts.
fn hex6(colour: Rgba) -> String {
    format!("#{:02x}{:02x}{:02x}", colour.r, colour.g, colour.b)
}

/// The raw text the draft holds for `role`, if it sets one.
fn raw_colour(draft: &Draft, role: PaletteRole) -> Option<String> {
    let source = draft.source().ok()?;
    let (mut doc, _) = format::read(&source).ok()?;
    doc.palette.as_mut()?.slot(role).clone()
}

fn contrast_rows(theme: &Theme) -> Vec<ContrastRow> {
    contrast::check(theme)
        .into_iter()
        .map(|r| ContrastRow {
            pair: r.pair.to_string(),
            ratio: r.ratio,
            min: r.min,
            pass: r.pass,
            decorative: r.decorative,
        })
        .collect()
}

fn effects_of(theme: &Theme) -> Vec<(String, f64)> {
    let e = &theme.effects;
    vec![
        ("blur".to_owned(), f64::from(e.blur_px)),
        ("vignette".to_owned(), e.vignette),
        ("grain".to_owned(), e.grain),
        ("glow".to_owned(), e.glow),
    ]
}

/// The editor's view of a draft. Each colour is the draft's resolved colour;
/// a role whose draft value is not a colour shows the base theme's. The
/// preview's error is shown, and its contrast table left empty.
fn editor_view(state: &Editing, catalogue: &Catalogue) -> EditorView {
    let base = catalogue.resolve(&state.base).ok().map(|r| r.theme);
    let preview = state.draft.preview(catalogue);
    let colours = PaletteRole::ALL
        .iter()
        .map(|&role| {
            let colour = match &preview {
                Ok(resolved) => resolved.theme.palette.get(role),
                Err(_) => raw_colour(&state.draft, role)
                    .and_then(|raw| Rgba::parse(&raw))
                    .or_else(|| base.as_ref().map(|t| t.palette.get(role)))
                    .unwrap_or(UNRESOLVED),
            };
            (role.label().to_owned(), hex6(colour))
        })
        .collect();
    let shown: Option<&Theme> = match &preview {
        Ok(resolved) => Some(&resolved.theme),
        Err(_) => base.as_ref(),
    };
    EditorView {
        name: state.draft.name().unwrap_or_default().to_owned(),
        base: state.base.clone(),
        colours,
        density: shown
            .map(|t| t.density.label().to_owned())
            .unwrap_or_default(),
        ui_font: shown.map(|t| t.ui.label().to_owned()).unwrap_or_default(),
        effects: shown.map(effects_of).unwrap_or_default(),
        contrast: preview
            .as_ref()
            .map(|resolved| contrast_rows(&resolved.theme))
            .unwrap_or_default(),
        error: preview.as_ref().err().map(ToString::to_string),
    }
}

/// An edit in progress: the draft and the theme it extends.
#[derive(Clone)]
struct Editing {
    draft: Draft,
    base: String,
}

/// Navigation that replaces the current history entry, for redirects.
#[derive(Clone, Copy)]
struct Replace(EventHandler<Route>);

/// What screens share: the signed-in API (none until resumed) and the
/// machine, so a `revoked` answer can send the app to sign-in.
#[derive(Clone, Copy)]
pub(super) struct Ctx {
    pub(super) api: Signal<Option<NativeApi>>,
    pub(super) machine: Signal<Machine>,
}

#[component]
fn NativeApp(route: Route) -> Element {
    let mut machine = use_signal(Machine::new);
    let mut api = use_signal(|| None::<NativeApi>);
    let mut task = use_signal(|| None::<dioxus::core::Task>);
    let mut started = use_signal(|| None::<u32>);
    use_context_provider(|| Ctx { api, machine });
    let nav = use_context::<Nav>();

    use_effect(move || {
        let (phase, epoch, delay) = {
            let m = machine.read();
            (m.phase().clone(), m.epoch(), m.delay_ms())
        };
        match phase {
            Phase::Connecting if *started.peek() != Some(epoch) => {
                started.set(Some(epoch));
                if let Some(old) = task.write().take() {
                    old.cancel();
                }
                // A reconnect keeps the old API until the new one resumes,
                // so the screens keep what they loaded.
                task.set(Some(spawn(connect(machine, api, epoch, delay))));
            }
            Phase::Connecting | Phase::Ready => {}
            _ => {
                if let Some(old) = task.write().take() {
                    old.cancel();
                }
                if api.peek().is_some() {
                    api.set(None);
                    platform::forget_opt_in();
                }
            }
        }
    });

    // Ready and reconnecting render through this one node, so a dropped
    // socket does not remount the screen (and its playing video).
    if machine.read().shows_screens() {
        let notice = {
            let m = machine.read();
            if m.reconnecting() {
                Some("Connection lost. Reconnecting…".to_owned())
            } else {
                m.notice().map(str::to_owned)
            }
        };
        return rsx! {
            Shell {
                on_sign_out: move |_| {
                    if machine.peek().signing_out() {
                        return;
                    }
                    machine.write().apply(Event::SignOutSubmitted);
                    spawn(async move {
                        let reply = platform::sign_out().await;
                        machine.write().apply(Event::SignOutAnswered(reply));
                    });
                },
                if let Some(message) = notice {
                    ErrorNote { message }
                }
                Screen { route }
            }
        };
    }
    let phase = machine.read().phase().clone();
    match phase {
        Phase::Connecting | Phase::Ready => rsx! {
            StatusView {
                title: "Connecting…",
                detail: "Opening a connection to the server.",
                action: None,
            }
        },
        Phase::NotEnabled => rsx! {
            StatusView {
                title: "This interface is not enabled",
                detail: "The server has the native API turned off \
                         ([server.native_api].enabled). The existing screens are under Manage.",
                action: Some(("Open Manage".to_string(), EventHandler::new(move |_| nav.0.call(Route::Manage)))),
            }
        },
        Phase::Unreachable { detail } => rsx! {
            StatusView {
                title: "Cannot connect",
                detail,
                action: Some(("Try again".to_string(), EventHandler::new(move |_| machine.write().apply(Event::Retry)))),
            }
        },
        Phase::SignIn { error, busy } => rsx! {
            SignInView {
                error,
                busy,
                on_submit: move |(name, password): (String, String)| {
                    machine.write().apply(Event::SignInSubmitted);
                    spawn(async move {
                        let reply = platform::sign_in(&name, &password).await;
                        machine.write().apply(Event::SignInAnswered(reply));
                    });
                },
            }
        },
    }
}

/// One connection attempt: wait out the backoff, open and resume, then
/// report the socket's close.
async fn connect(
    mut machine: Signal<Machine>,
    mut api: Signal<Option<NativeApi>>,
    epoch: u32,
    delay_ms: u32,
) {
    platform::sleep_ms(delay_ms).await;
    let event = match platform::open(&call("connect", "resume")).await {
        Ok(mut open) => {
            api.set(Some(open.api.clone()));
            platform::install_telemetry(&open.api);
            machine.write().apply(Event::Resumed { epoch });
            let reason = platform::closed(&mut open.conn).await;
            Event::Closed { epoch, reason }
        }
        Err(OpenError::NotOpened(probe)) => Event::NotOpened { epoch, probe },
        Err(OpenError::NotSignedIn) => Event::ResumeRefused { epoch },
        Err(OpenError::Resume(error)) => Event::ResumeFailed { epoch, error },
    };
    machine.write().apply(event);
}

pub(super) fn call(route: &str, action: &str) -> CallInfo {
    CallInfo {
        ui_route: route.into(),
        ui_action: action.into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
        device: "web".into(),
        ..CallInfo::default()
    }
}

/// A finished load: the value, or the message to show. A `revoked` answer
/// also sends the app to sign-in.
fn settle<T>(mut machine: Signal<Machine>, result: Result<T, ClientError>) -> Result<T, String> {
    result.map_err(|e| {
        if e.needs_sign_in() {
            machine.write().apply(Event::NeedsSignIn);
        }
        e.to_string()
    })
}

#[component]
fn Screen(route: Route) -> Element {
    match route {
        Route::Home => rsx! { HomeScreen {} },
        Route::Browse(id) => rsx! { BrowseScreen { id } },
        Route::Title(id) => rsx! { TitleScreen { id } },
        // Another item (the next episode) mounts a new playback rather than
        // re-rendering this one's.
        Route::Watch(id) => remount_on(&id.to_string(), rsx! { WatchScreen { id } }),
        Route::SignIn => rsx! { Redirect { to: Route::Home } },
        // `Root` mounts the older panes for this route.
        Route::Manage => rsx! { Redirect { to: Route::Manage } },
        Route::Appearance => rsx! { AppearanceScreen {} },
        Route::Find => rsx! { FindScreen {} },
        Route::NotFound(path) => rsx! { ErrorNote { message: format!("Nothing at {path}.") } },
    }
}

/// The find screen: each edit of the box starts a debounced search, and only
/// the reply to the newest edit is shown.
#[component]
fn FindScreen() -> Element {
    let ctx = use_context::<Ctx>();
    let mut text = use_signal(String::new);
    let mut find = use_signal(Find::default);
    let mut hits = use_signal(|| Hits::Idle);
    let on_input = move |value: String| {
        text.set(value.clone());
        let Some(generation) = find.write().edit(&value) else {
            hits.set(Hits::Idle);
            return;
        };
        spawn(async move {
            platform::sleep_ms(DEBOUNCE_MS).await;
            if !find.peek().is_current(generation) {
                return;
            }
            let Some(api) = ctx.api.peek().clone() else {
                return;
            };
            let term = find.peek().term().to_owned();
            let reply = api
                .search(&call("find", "search"), &term, RESULT_LIMIT)
                .await;
            if !find.peek().is_current(generation) {
                return;
            }
            match settle(ctx.machine, reply) {
                Ok(page) => hits.set(Hits::Found(page)),
                Err(message) => hits.set(Hits::Failed(message)),
            }
        });
    };
    rsx! { FindView { text: text(), hits: hits(), on_input } }
}

/// Replace the current route once mounted (signed in at `/signin`).
#[component]
fn Redirect(to: Route) -> Element {
    let replace = use_context::<Replace>();
    use_effect(move || replace.0.call(to.clone()));
    rsx! { Loading {} }
}

/// The Appearance page: the theme gallery, imports, and the editor, mapped
/// onto the shared theme state.
#[component]
fn AppearanceScreen() -> Element {
    let themes = use_context::<Themes>();
    let mut editing = use_signal(|| None::<Editing>);
    let notice = use_signal(|| None::<Notice>);
    let ctx = use_context::<Ctx>();
    let mut diagnostics = use_signal(platform::opted_in);
    // Bumped when a save fails: the box is keyed on it, so the browser's
    // flipped checkbox is rebuilt from the server value.
    let mut diagnostics_gen = use_signal(|| 0_u32);
    // The box shows the server's choice, not whatever was cached at sign-in.
    use_future(move || async move {
        let api = ctx.api.peek().clone();
        if let Some(api) = api {
            if let Ok(on) = platform::load_opted_in(&api).await {
                diagnostics.set(on);
            }
        }
    });
    let cards = use_memo(move || theme_cards(&themes.catalogue.read()));
    // Leaving the page mid-edit must put the active theme back.
    use_drop(move || {
        if editing.peek().is_some() {
            themes.apply_active();
        }
    });

    let applied = themes.applied.read().clone();
    let mut list: Vec<ThemeCard> = cards.read().clone();
    for card in list.iter_mut() {
        card.active = card.name == applied;
    }
    let auto = *themes.active.read() == device::AUTO;
    let editor = editing
        .read()
        .as_ref()
        .map(|state| editor_view(state, &themes.catalogue.read()));
    let notice_now = notice.read().clone();
    let diagnostics_on = *diagnostics.read();
    let diagnostics_key = *diagnostics_gen.read();

    rsx! {
        AppearanceView {
            themes: list,
            auto,
            editor,
            notice: notice_now,
            diagnostics: diagnostics_on,
            diagnostics_key,
            on_diagnostics: move |on: bool| {
                let api = ctx.api.peek().clone();
                let server_before = *diagnostics.peek();
                spawn(async move {
                    let saved = match api {
                        Some(api) => platform::set_opted_in(&api, on).await.map(|()| on),
                        None => Err("not signed in".to_owned()),
                    };
                    if let Err(e) = &saved {
                        notify(
                            notice,
                            format!("Could not save the diagnostics choice: {e}"),
                            true,
                        );
                        diagnostics_gen.with_mut(|g| *g = g.wrapping_add(1));
                    }
                    diagnostics.set(diagnostics::shown_after_save(saved, server_before));
                });
            },
            on_pick: move |name: String| {
                editing.set(None);
                themes.choose(name.clone());
                notify(notice, format!("Applied {name}"), false);
            },
            on_auto: move |()| {
                editing.set(None);
                themes.choose(device::AUTO.to_owned());
                notify(notice, "Following the system".to_owned(), false);
            },
            on_customise: move |base: String| customise(themes, editing, notice, base),
            on_edit_custom: move |name: String| edit_custom(themes, editing, notice, &name),
            on_delete: move |name: String| delete(themes, editing, notice, &name),
            on_export: move |name: String| export(themes, notice, &name),
            on_import_file: move |()| import_file(themes, notice),
            on_import_paste: move |text: String| import(themes, notice, text),
            on_edit: move |edit: Edit| edit_draft(themes, editing, edit),
            on_save: move |()| save_editor(themes, editing, notice),
            on_cancel: move |()| cancel(themes, editing, notice),
        }
    }
}

/// Renders a load's state: loading, its error, or `view` of its value.
fn loaded<T>(state: Option<&Result<T, String>>, view: impl FnOnce(&T) -> Element) -> Element {
    match state {
        None => rsx! { Loading {} },
        Some(Err(message)) => rsx! { ErrorNote { message: message.clone() } },
        Some(Ok(value)) => view(value),
    }
}

#[component]
fn HomeScreen() -> Element {
    let ctx = use_context::<Ctx>();
    let home = use_resource(move || {
        let api = ctx.api.read().clone();
        async move {
            let api = api?;
            Some(settle(ctx.machine, api.home(&call("home", "load")).await))
        }
    });
    let state = home.read();
    loaded(state.as_ref().and_then(Option::as_ref), |home: &Home| {
        rsx! { HomeView { home: home.clone() } }
    })
}

#[component]
fn BrowseScreen(id: ItemRef) -> Element {
    let ctx = use_context::<Ctx>();
    // The screen stays mounted when a tile or season tab opens another
    // parent, so the load follows `id` instead of running once.
    let listing = use_resource(use_reactive!(|(id,)| {
        let api = ctx.api.read().clone();
        async move {
            let api = api?;
            let result = browse::load(&api, &call("browse", "load"), id, |line| {
                platform::warn(&line)
            })
            .await;
            Some(settle(ctx.machine, result))
        }
    }));
    let state = listing.read();
    loaded(
        state.as_ref().and_then(Option::as_ref),
        |browse: &browse::Browse| {
            rsx! { browse::BrowseLayout { browse: browse.clone() } }
        },
    )
}

#[component]
fn TitleScreen(id: ItemRef) -> Element {
    let ctx = use_context::<Ctx>();
    let detail = use_resource(move || {
        let api = ctx.api.read().clone();
        async move {
            let api = api?;
            Some(settle(
                ctx.machine,
                api.detail(&call("title", "load"), id).await,
            ))
        }
    });
    let state = detail.read();
    loaded(
        state.as_ref().and_then(Option::as_ref),
        |detail: &Detail| {
            rsx! { TitleView { detail: detail.clone() } }
        },
    )
}

/// A negotiated playback, ready for the player.
struct Playing {
    title: String,
    kind: ItemKind,
    image: Option<String>,
    url: String,
    start_ms: u64,
    engine: Engine,
    /// The server's play session; a new one remounts the player.
    session: String,
    grant: NativeGrant,
    detail: Detail,
    /// The tracks the server started this playback on.
    audio: Option<u32>,
    subtitle: Option<u32>,
}

impl Playing {
    /// Whether the stream has its subtitle drawn in by the server.
    fn burned(&self) -> bool {
        self.subtitle.is_some_and(|i| {
            self.detail
                .subtitle_tracks
                .iter()
                .any(|t| t.index == i && t.vtt_url.is_none())
        })
    }
}

/// The playback props, compared by identity: a new negotiation is a new
/// player.
#[derive(Clone)]
struct PlayingRef(Rc<Playing>);

impl PartialEq for PlayingRef {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

#[component]
fn WatchScreen(id: ItemRef) -> Element {
    let ctx = use_context::<Ctx>();
    // Where the player was, so a renegotiation after a reconnect or a track
    // change carries on from there rather than from the stored position.
    let last_ms = use_hook(|| Rc::new(Cell::new(0u64)));
    // The tracks asked for; changing them negotiates a new playback.
    let mut picks = use_signal(TrackPicks::default);
    let resume_from = Rc::clone(&last_ms);
    // The episodes either side follow the item, not the playback, so a track
    // change that renegotiates does not look them up again.
    let near = use_resource(move || {
        let api = ctx.api.read().clone();
        async move {
            let api = api?;
            let info = call("watch", "neighbours");
            let result = async {
                let episode = api.detail(&info, id).await?.item;
                if episode.kind != ItemKind::Episode {
                    return Ok(Neighbours::default());
                }
                browse::neighbours(&api, &info, &episode).await
            }
            .await;
            result
                .map_err(|e| {
                    platform::warn(&format!("pharos: episode neighbours unavailable: {e}"))
                })
                .ok()
        }
    });
    let playing = use_resource(move || {
        let api = ctx.api.read().clone();
        let resume_from = Rc::clone(&resume_from);
        let asked = picks();
        async move {
            let api = api?;
            let info = call("watch", "play");
            let result = async {
                let detail = api.detail(&info, id).await?;
                let (decision, grant) = api
                    .play(&info, id, &platform::capabilities(), asked)
                    .await?;
                Ok::<_, ClientError>((detail, decision, grant))
            }
            .await;
            Some(
                settle(ctx.machine, result).and_then(|(detail, decision, grant)| {
                    let engine = choose_engine(
                        decision.is_hls(),
                        platform::plays_hls(),
                        platform::has_mse(),
                    );
                    platform::info(&format!(
                        "pharos: playback method={} engine={engine:?} block={} downgrade={} audio={:?} subtitle={:?} url={}",
                        decision.method,
                        decision.direct_play_block,
                        decision.downgrade,
                        decision.audio,
                        decision.subtitle,
                        decision.stream_url
                    ));
                    if engine == Engine::Unsupported {
                        return Err(
                            "This title needs HLS, which this browser cannot play.".to_owned()
                        );
                    }
                    let start_ms = match resume_from.get() {
                        0 => start_ms(detail.item.position_ms, detail.item.played),
                        here => here,
                    };
                    Ok(PlayingRef(Rc::new(Playing {
                        title: detail.item.name.clone(),
                        kind: detail.item.kind,
                        image: detail.item.image.clone(),
                        url: decision.stream_url,
                        start_ms,
                        engine,
                        session: decision.play_session,
                        grant,
                        detail,
                        audio: decision.audio,
                        subtitle: decision.subtitle,
                    })))
                }),
            )
        }
    });
    let state = playing.read();
    loaded(state.as_ref().and_then(Option::as_ref), |p: &PlayingRef| {
        remount_on(
            &p.0.session,
            rsx! {
                Player {
                    playing: p.clone(),
                    last_ms: LastMs(Rc::clone(&last_ms)),
                    near: near.read().flatten().unwrap_or_default(),
                    on_renegotiate: move |next| picks.set(next),
                }
            },
        )
    })
}

/// Mounts `body` afresh whenever `key` changes, dropping its hooks. Dioxus
/// compares keys only between siblings of a list, so a lone keyed node keeps
/// its old hooks and the new props never reach them; a one-item list does.
fn remount_on(key: &str, body: Element) -> Element {
    rsx! {
        for key in std::iter::once(key) {
            Fragment { key: "{key}", {body.clone()} }
        }
    }
}

/// The player's last known position, shared with its screen.
#[derive(Clone)]
struct LastMs(Rc<Cell<u64>>);

impl PartialEq for LastMs {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

fn choice(index: Option<u32>) -> TrackChoice {
    index.map_or(TrackChoice::Off, TrackChoice::Index)
}

/// Attaches the engine, draws the controls and reports progress: on play,
/// pause and seek, every 10 s while playing, and one stop when the video
/// ends, the player unmounts or the page is hidden. A track change the
/// `<video>` cannot make itself asks the screen for a new playback.
#[component]
fn Player(
    playing: PlayingRef,
    last_ms: LastMs,
    near: Neighbours,
    on_renegotiate: EventHandler<TrackPicks>,
) -> Element {
    let ctx = use_context::<Ctx>();
    let p = playing.0;
    let clock = use_hook(|| Rc::new(RefCell::new(ReportClock::new())));
    let engine = use_hook(|| Rc::new(RefCell::new(None::<Box<dyn PlaybackEngine>>)));
    let nav = use_context::<Nav>();
    let replace = use_context::<Replace>();
    let mut failed = use_signal(|| None::<String>);
    let mut media = use_signal(MediaState::default);
    let mut hover_ms = use_signal(|| None::<u64>);
    // The subtitle on screen: the server's pick, then the viewer's.
    let mut subtitle = use_signal(|| p.subtitle);
    // The subtitle the viewer switched to in place, which reports carry so
    // the next playback starts on it.
    let switched = use_hook(|| Rc::new(Cell::new(None::<TrackChoice>)));

    let stop = {
        let (p, clock, last_ms) = (Rc::clone(&p), Rc::clone(&clock), Rc::clone(&last_ms.0));
        move || {
            if !clock.borrow_mut().stop() {
                return;
            }
            let grant = p.grant.clone();
            let at = last_ms.get();
            platform::spawn_detached(async move {
                if let Err(e) = grant.stop(&call("watch", "stop"), at).await {
                    platform::warn(&format!("pharos: playback stop failed: {e}"));
                }
            });
        }
    };
    use_hook({
        let stop = stop.clone();
        move || Rc::new(platform::on_page_hide(stop))
    });
    use_drop({
        let (engine, stop) = (Rc::clone(&engine), stop.clone());
        move || {
            engine.borrow_mut().take();
            stop();
        }
    });
    // The engines call back from browser events, outside the Dioxus runtime.
    let fatal = use_coroutine({
        let stop = stop.clone();
        move |mut reasons: UnboundedReceiver<String>| {
            let stop = stop.clone();
            async move {
                while let Some(reason) = reasons.next().await {
                    stop();
                    failed.set(Some(format!("Playback failed: {reason}")));
                }
            }
        }
    });
    use_effect({
        let (p, engine) = (Rc::clone(&p), Rc::clone(&engine));
        move || {
            let (p, engine) = (Rc::clone(&p), Rc::clone(&engine));
            spawn(async move {
                let on_fatal: OnFatal = Rc::new(move |reason| fatal.send(reason));
                let attached = match platform::engine(p.engine).await {
                    Ok(mut e) => e.attach(VIDEO_ID, &p.url, p.start_ms, on_fatal).map(|()| e),
                    Err(e) => Err(e),
                };
                match attached {
                    Ok(e) => *engine.borrow_mut() = Some(e),
                    Err(e) => failed.set(Some(format!("Could not start playback: {e}"))),
                }
            });
        }
    });
    // A burned subtitle is in the picture, not a text track.
    let burned = p.burned();
    use_effect(move || {
        let shown = if burned { None } else { subtitle() };
        let id = shown.map(text_track_id);
        platform::show_text_track(VIDEO_ID, id.as_deref());
    });

    let on_media = {
        let (p, clock, last_ms, switched) = (
            Rc::clone(&p),
            Rc::clone(&clock),
            Rc::clone(&last_ms.0),
            Rc::clone(&switched),
        );
        move |event: MediaEvent| {
            media.set(platform::media_state(VIDEO_ID));
            if event == MediaEvent::Changed && !burned {
                // Text tracks load with the metadata; show the picked one.
                let shown: Option<u32> = *subtitle.peek();
                let id = shown.map(text_track_id);
                platform::show_text_track(VIDEO_ID, id.as_deref());
            }
            let at = platform::position_ms(VIDEO_ID);
            if at > 0 {
                last_ms.set(at);
            }
            let Some(send) = clock.borrow_mut().on(platform::now_ms(), event) else {
                return;
            };
            if send == Outgoing::Stop {
                stop();
                return;
            }
            let Outgoing::Report { paused } = send else {
                return;
            };
            let grant = p.grant.clone();
            let at = last_ms.get();
            let tracks = TrackPicks {
                audio: TrackChoice::Default,
                subtitle: switched.get().unwrap_or_default(),
            };
            spawn(async move {
                let result = grant
                    .report(&call("watch", "report"), at, paused, tracks)
                    .await;
                if let Err(e) = settle(ctx.machine, result) {
                    platform::warn(&format!("pharos: progress report failed: {e}"));
                }
            });
        }
    };

    let on_control = {
        let p = Rc::clone(&p);
        move |control: Control| {
            let state = *media.peek();
            let total = match state.duration_ms {
                0 => p.detail.item.runtime_ms,
                d => d,
            };
            match control {
                Control::TogglePlay => platform::toggle_play(VIDEO_ID),
                // An unknown length (0) clamps nothing rather than everything.
                Control::Seek(ms) if total > 0 => platform::seek_ms(VIDEO_ID, ms.min(total)),
                Control::Seek(ms) => platform::seek_ms(VIDEO_ID, ms),
                Control::SeekAt(x) => {
                    if let Some(ms) = position_at(x, platform::element_width(SEEK_ID), total) {
                        platform::seek_ms(VIDEO_ID, ms);
                    }
                }
                Control::Hover(x) => hover_ms
                    .set(x.and_then(|x| position_at(x, platform::element_width(SEEK_ID), total))),
                Control::Volume { volume, muted } => {
                    platform::set_volume(VIDEO_ID, volume, muted);
                }
                Control::Fullscreen => platform::toggle_fullscreen(PLAYER_ID),
                // In place of this episode, so Back leaves the show rather than
                // replaying every episode watched.
                Control::Play(id) => replace.0.call(Route::Watch(id)),
                Control::Back => {
                    if !platform::go_back() {
                        nav.0.call(Route::Home);
                    }
                }
                Control::Audio(index) => {
                    if p.audio != Some(index) {
                        on_renegotiate.call(TrackPicks {
                            audio: TrackChoice::Index(index),
                            subtitle: choice(*subtitle.peek()),
                        });
                    }
                }
                Control::Subtitle(to) => {
                    let track =
                        to.and_then(|i| p.detail.subtitle_tracks.iter().find(|t| t.index == i));
                    match subtitle_switch(p.burned(), track) {
                        SubtitleSwitch::InPlace => {
                            switched.set(Some(choice(to)));
                            subtitle.set(to);
                        }
                        SubtitleSwitch::Renegotiate => on_renegotiate.call(TrackPicks {
                            audio: p.audio.map_or(TrackChoice::Default, TrackChoice::Index),
                            subtitle: choice(to),
                        }),
                    }
                }
            }
        }
    };

    let controls = Controls {
        media: media(),
        runtime_ms: p.detail.item.runtime_ms,
        chapters: p.detail.chapters.clone(),
        segments: p.detail.segments.clone(),
        preview: preview_set(&p.detail.trickplay).cloned(),
        hover_ms: hover_ms(),
        audio: p.detail.audio_tracks.clone(),
        subtitles: p.detail.subtitle_tracks.clone(),
        audio_pick: p.audio,
        subtitle_pick: subtitle(),
        previous: near.previous,
        next: near.next,
    };
    rsx! {
        if let Some(message) = failed() {
            ErrorNote { message }
        }
        if p.kind == ItemKind::Audio {
            ListenView { title: p.title.clone(), image: p.image.clone(), on_media }
        } else {
            WatchView { title: p.title.clone(), controls, on_media, on_control }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Counts the mounts of `remount_on`'s body as the key changes.
    fn mounts_across(keys: &[&str]) -> u32 {
        #[component]
        fn Probe(mounts: Rc<Cell<u32>>) -> Element {
            use_hook(|| mounts.set(mounts.get() + 1));
            rsx! {}
        }
        #[derive(Clone)]
        struct Props {
            key: Rc<RefCell<String>>,
            mounts: Rc<Cell<u32>>,
        }
        let props = Props {
            key: Rc::new(RefCell::new(keys[0].to_owned())),
            mounts: Rc::new(Cell::new(0)),
        };
        let mut dom = VirtualDom::new_with_props(
            |p: Props| {
                let key = p.key.borrow().clone();
                remount_on(&key, rsx! { Probe { mounts: Rc::clone(&p.mounts) } })
            },
            props.clone(),
        );
        dom.rebuild_in_place();
        for key in &keys[1..] {
            *props.key.borrow_mut() = (*key).to_owned();
            dom.mark_dirty(ScopeId::APP);
            dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        }
        props.mounts.get()
    }

    #[test]
    fn a_new_play_session_mounts_a_new_player() {
        assert_eq!(mounts_across(&["a", "b", "c"]), 3);
    }

    #[test]
    fn the_same_play_session_keeps_its_player() {
        assert_eq!(mounts_across(&["a", "a"]), 1);
    }

    #[test]
    fn the_app_starts_by_connecting() {
        let mut dom = VirtualDom::new(Root);
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("Connecting…"), "{html}");
    }

    #[test]
    fn the_device_starts_on_the_default_theme_when_nothing_is_stored() {
        let boot = boot_theme();
        assert_eq!(boot.active, theme::DEFAULT);
        assert_eq!(boot.applied, theme::DEFAULT);
    }

    #[test]
    fn a_theme_that_does_not_resolve_gets_no_card() {
        let mut catalogue = theme::shipped();
        catalogue
            .insert("[meta]\nname = \"Broken\"\nschema = 1\nextends = \"Nowhere\"\n".to_owned())
            .unwrap();
        let cards = theme_cards(&catalogue);
        assert_eq!(cards.len(), 7);
        assert!(cards.iter().all(|c| c.name != "Broken"));
    }

    #[test]
    fn a_card_shows_the_resolved_bg_surface_accent_and_fg() {
        let catalogue = theme::shipped();
        let resolved = catalogue.resolve("Kali Dark").unwrap().theme;
        let card = theme_cards(&catalogue)
            .into_iter()
            .find(|c| c.name == "Kali Dark")
            .unwrap();
        let p = resolved.palette;
        assert_eq!(
            card.swatches,
            [p.bg, p.surface, p.accent, p.fg]
                .iter()
                .map(Rgba::to_css)
                .collect::<Vec<_>>()
        );
        assert!(card.shipped);
    }

    #[test]
    fn a_draft_colour_that_is_not_a_colour_shows_the_base_colour_and_the_error() {
        let catalogue = theme::shipped();
        let base = catalogue.resolve("Kali Dark").unwrap().theme;
        let mut draft = Draft::new("Mine", "Kali Dark");
        draft.set_colour(PaletteRole::Accent, "nope");
        let state = Editing {
            draft,
            base: "Kali Dark".into(),
        };
        let view = editor_view(&state, &catalogue);
        let accent = view
            .colours
            .iter()
            .find(|(role, _)| role == "accent")
            .unwrap();
        assert_eq!(accent.1, hex6(base.palette.accent));
        assert!(
            view.error
                .as_deref()
                .unwrap_or("")
                .contains("palette.accent"),
            "{view:?}"
        );
        assert!(view.contrast.is_empty());
    }

    #[test]
    fn a_valid_draft_colour_shows_its_resolved_hex() {
        let catalogue = theme::shipped();
        let mut draft = Draft::new("Mine", "Kali Dark");
        draft.set_colour(PaletteRole::Accent, "#FF00FF");
        let view = editor_view(
            &Editing {
                draft,
                base: "Kali Dark".into(),
            },
            &catalogue,
        );
        let accent = view
            .colours
            .iter()
            .find(|(role, _)| role == "accent")
            .unwrap();
        assert_eq!(accent.1, "#ff00ff");
        assert_eq!(view.error, None);
        assert!(!view.contrast.is_empty());
    }

    #[test]
    fn editor_changes_land_on_the_draft_and_unknown_fields_are_refused() {
        let mut draft = Draft::new("Mine", "Kali Dark");
        apply_edit(&mut draft, Edit::Colour("accent".into(), "#00ff00".into())).unwrap();
        apply_edit(&mut draft, Edit::Effect("blur".into(), 3.0)).unwrap();
        apply_edit(&mut draft, Edit::Density("compact".into())).unwrap();
        apply_edit(&mut draft, Edit::Name("Renamed".into())).unwrap();
        let resolved = draft.preview(&theme::shipped()).unwrap();
        assert_eq!(resolved.theme.palette.accent.g, 255);
        assert_eq!(resolved.theme.effects.blur_px, 3);
        assert_eq!(resolved.theme.name, "Renamed");
        assert!(apply_edit(&mut draft, Edit::Colour("glitter".into(), "#000000".into())).is_err());
        assert!(apply_edit(&mut draft, Edit::Effect("sparkle".into(), 1.0)).is_err());
    }

    #[test]
    fn a_new_custom_theme_takes_the_first_free_name() {
        let mut catalogue = theme::shipped();
        assert_eq!(fresh_name(&catalogue, "Kali Dark"), "My Kali Dark");
        catalogue
            .insert("[meta]\nname = \"My Kali Dark\"\nschema = 1\n".to_owned())
            .unwrap();
        assert_eq!(fresh_name(&catalogue, "Kali Dark"), "My Kali Dark 2");
    }

    #[test]
    fn a_save_over_the_size_limit_is_refused_with_its_reason() {
        let refusal = device::save(
            &mut theme::shipped(),
            "x".repeat(doc::MAX_BYTES + 1),
            Source::Import,
        )
        .unwrap_err();
        assert!(refusal.reason.contains("limit"), "{}", refusal.reason);
    }

    #[component]
    fn HarnessRoot() -> Element {
        let themes = Themes {
            catalogue: use_signal(theme::shipped),
            active: use_signal(|| theme::DEFAULT.to_owned()),
            applied: use_signal(|| theme::DEFAULT.to_owned()),
        };
        use_context_provider(|| themes);
        let ctx = Ctx {
            api: use_signal(|| None::<NativeApi>),
            machine: use_signal(Machine::new),
        };
        use_context_provider(|| ctx);
        rsx! { AppearanceScreen {} }
    }

    #[test]
    fn the_appearance_screen_renders_seven_cards_with_the_applied_one_marked() {
        let mut dom = VirtualDom::new(HarnessRoot);
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert_eq!(
            html.matches("class=\"pharos-theme-card\"").count(),
            7,
            "{html}"
        );
        let marked = html
            .split("<li")
            .find(|chunk| chunk.contains("aria-current=\"true\""))
            .unwrap_or("");
        assert!(marked.contains("data-theme=\"pharos-night\""), "{html}");
        assert!(html.contains("class=\"pharos-swatch\""), "{html}");
    }
}
