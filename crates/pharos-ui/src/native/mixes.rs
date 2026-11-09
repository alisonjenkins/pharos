//! Dynamic mixes in the player (spec 024 slice 2, B441/V382).
//!
//! A mix is a set of rules the server picks songs for (`Library.mix`). The
//! UI holds the mix that is on, keeps the queue topped up from it, and lets
//! the listener start a preset, a saved mix or "more like this song", stop
//! it, or build and save their own. Mix songs go in the queue as
//! `Source::Mix`, after anything the listener added; switching or stopping a
//! mix takes out only those.

use std::collections::HashMap;

use dioxus::prelude::*;
use pharos_client::api::{ItemRef, NativeApi};
use pharos_client::mix::{
    decode_saved, encode_saved, presets, radio, MixBatch, MixSpec, Rule, SavedMix, Seed, Test,
    Weight, SAVED_KEY,
};

use super::app::{call, remount_on, Ctx, Flash};
use super::group_app::Together;
use super::listen::Icon;
use super::platform;
use super::queue::{Queue, QueueAction, QueueCtx, Source, Track};
use super::route::Route;
use super::views::Nav;

/// Fewer upcoming songs than this and the mix asks for more.
pub const LOW_WATER: usize = 5;
/// How many songs one ask brings.
pub const BATCH: u32 = 10;

/// A mix: a die showing five, as the auto playlist's dice in Amarok.
pub const ICON_MIX: &str = "M6 3h12a3 3 0 0 1 3 3v12a3 3 0 0 1-3 3H6a3 3 0 0 1-3-3V6a3 3 0 0 1 3-3Zm0 2a1 1 0 0 0-1 1v12a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1V6a1 1 0 0 0-1-1H6Zm1 3.5a1.5 1.5 0 1 0 3 0 1.5 1.5 0 1 0-3 0Zm7 0a1.5 1.5 0 1 0 3 0 1.5 1.5 0 1 0-3 0ZM10.5 12a1.5 1.5 0 1 0 3 0 1.5 1.5 0 1 0-3 0ZM7 15.5a1.5 1.5 0 1 0 3 0 1.5 1.5 0 1 0-3 0Zm7 0a1.5 1.5 0 1 0 3 0 1.5 1.5 0 1 0-3 0Z";

/// The mix that is on.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveMix {
    pub name: String,
    pub spec: MixSpec,
    /// Bumped on every start, so an answer for an older mix is dropped.
    pub generation: u64,
    pub asking: bool,
    /// The server said the mix has no more songs.
    pub exhausted: bool,
    /// The last ask failed; the mix waits for Retry.
    pub error: Option<String>,
    /// Play the first song of the first batch as soon as it comes.
    pub play_first: bool,
    /// Why the mix picked each song, as the server said.
    pub reasons: HashMap<ItemRef, Vec<String>>,
}

/// The app-wide mix, provided beside the queue at the app's root.
#[derive(Clone, Copy)]
pub struct MixCtx(pub Signal<Option<ActiveMix>>);

/// Whether the mix should ask for more now: when the queue runs low, or
/// at once for a mix that plays its first song now, however much the
/// listener has queued.
pub fn needs_more(queue: &Queue, mix: &ActiveMix) -> bool {
    !mix.asking
        && !mix.exhausted
        && mix.error.is_none()
        && (mix.play_first || queue.upcoming().len() < LOW_WATER)
}

/// The rest of a started song's album that the queue takes: none while a
/// mix is on, because the mix decides what follows (V382). `None` (the
/// album could not be read) stays `None`.
pub fn album_rest<T>(mixing: bool, rest: Option<&[T]>) -> Option<&[T]> {
    if mixing {
        rest.map(|_| &[][..])
    } else {
        rest
    }
}

/// What the server reads as heard or queued, oldest first: the songs
/// started (the queue keeps them newest first), then what is still queued.
pub fn history(queue: &Queue) -> Vec<ItemRef> {
    let mut ids: Vec<ItemRef> = queue.recent().collect();
    ids.reverse();
    ids.extend(queue.upcoming().iter().map(|e| e.id));
    ids
}

/// The console line for one ask.
pub fn top_up_line(name: &str, asked: u32, batch: &MixBatch) -> String {
    format!(
        "pharos: mix top-up mix={name:?} asked={asked} got={} exhausted={} relaxed={} eligible={}",
        batch.picks.len(),
        batch.exhausted,
        if batch.relaxed.is_empty() {
            "none".to_owned()
        } else {
            batch.relaxed.join(",")
        },
        batch.eligible
    )
}

/// Turn `spec` on as `name`, in place of any mix that was on: the old mix's
/// songs leave the queue and so do the album's (a mix decides what follows),
/// what the listener added stays. The top-up asks for the first batch.
pub fn start(
    queue: &mut Queue,
    mix: &mut Option<ActiveMix>,
    name: String,
    spec: MixSpec,
    play_now: bool,
) {
    queue.drop_source(Source::Mix);
    queue.drop_source(Source::Album);
    let generation = next_generation();
    platform::info(&format!(
        "pharos: mix start mix={name:?} rules={} seed={} play_now={play_now}",
        spec.rules.len(),
        seed_label(spec.seed)
    ));
    *mix = Some(ActiveMix {
        name,
        spec,
        generation,
        asking: false,
        exhausted: false,
        error: None,
        play_first: play_now,
        reasons: HashMap::new(),
    });
}

/// A number no earlier start had, for the app's life: it never resets, so
/// an answer for a stopped mix cannot pass for a later one's.
fn next_generation() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Queue a batch as `Source::Mix`. When `first` is to play now it goes to
/// the front, so starting it takes it out of the queue (songs leave from
/// the front) instead of leaving a copy behind what the listener added.
pub fn apply_batch(queue: &mut Queue, tracks: Vec<Track>, first: Option<ItemRef>) {
    queue.append(tracks, Source::Mix);
    let Some(first) = first else {
        return;
    };
    let key = queue
        .upcoming()
        .iter()
        .rev()
        .find(|e| e.id == first && e.source == Source::Mix)
        .map(|e| e.key);
    if let Some(key) = key {
        queue.apply(QueueAction::Move { key, to: 0 }, 0);
    }
}

/// Turn the mix off; its songs leave the queue.
pub fn stop(queue: &mut Queue, mix: &mut Option<ActiveMix>) {
    if let Some(m) = mix.take() {
        platform::info(&format!("pharos: mix stop mix={:?}", m.name));
    }
    queue.drop_source(Source::Mix);
}

fn seed_label(seed: Seed) -> &'static str {
    match seed {
        Seed::None => "none",
        Seed::Song(_) => "song",
        Seed::Follow => "follow",
    }
}

/// Keeps the queue topped up while a mix is on. Called once, at the app's
/// root, after the queue and mix contexts are provided.
pub fn use_top_up(
    api: Signal<Option<NativeApi>>,
    mut queue: Signal<Queue>,
    mut mix: Signal<Option<ActiveMix>>,
) {
    let nav = try_use_context::<Nav>();
    // Joining a SyncPlay group stops the mix: the group picks the songs, and
    // a mix left on would keep adding to a queue nobody plays from.
    let together = try_use_context::<Together>();
    use_effect(move || {
        let in_group = together.as_ref().is_some_and(|t| t.view.read().active);
        if in_group && mix.peek().is_some() {
            platform::info("pharos: mix stop reason=group");
            stop(&mut queue.write(), &mut mix.write());
        }
    });
    use_effect(move || {
        let want = {
            let q = queue.read();
            let m = mix.read();
            m.as_ref()
                .filter(|m| needs_more(&q, m))
                .map(|m| (m.name.clone(), m.spec.clone(), m.generation, history(&q)))
        };
        let Some((name, spec, generation, recent)) = want else {
            return;
        };
        // Read, not peeked: a mix started before the API connects asks once
        // it does.
        let Some(api) = api.read().clone() else {
            return;
        };
        if let Some(m) = mix.write().as_mut() {
            m.asking = true;
        }
        spawn(async move {
            let result = pharos_client::api::PharosApi::mix(
                &api,
                &call("listen", "mix"),
                &spec,
                &recent,
                BATCH,
            )
            .await;
            let mut first = None;
            {
                let mut guard = mix.write();
                // Stopped or switched while asking: this answer is stale.
                let Some(m) = guard.as_mut().filter(|m| m.generation == generation) else {
                    platform::info(&format!(
                        "pharos: mix top-up dropped mix={name:?} reason=stale"
                    ));
                    return;
                };
                m.asking = false;
                match result {
                    Ok(batch) => {
                        platform::info(&top_up_line(&name, BATCH, &batch));
                        // No songs is the end of the mix whatever the flag
                        // says: asking again would get none again.
                        m.exhausted = batch.exhausted || batch.picks.is_empty();
                        let tracks: Vec<Track> =
                            batch.picks.iter().map(|p| Track::from(&p.item)).collect();
                        for p in batch.picks {
                            m.reasons.insert(p.item.id, p.reasons);
                        }
                        if m.play_first {
                            m.play_first = false;
                            first = tracks.first().map(|t| t.id);
                        }
                        let mut q = queue.write();
                        apply_batch(&mut q, tracks, first);
                        // Keep reasons only for what is still queued.
                        m.reasons.retain(|id, _| q.contains(*id));
                    }
                    Err(e) => {
                        platform::warn(&format!(
                            "pharos: mix top-up failed mix={name:?} error={e}"
                        ));
                        m.error = Some(e.to_string());
                    }
                }
            }
            if let (Some(first), Some(nav)) = (first, nav) {
                nav.0.call(Route::Watch(first));
            }
        });
    });
}

/// The user's saved mixes from `Prefs`; none when nothing is stored yet.
async fn load_saved(api: &NativeApi) -> Result<Vec<SavedMix>, String> {
    let raw = api
        .pref(&call("listen", "mix_saved_read"), SAVED_KEY)
        .await
        .map_err(|e| e.to_string())?;
    match raw {
        None => Ok(Vec::new()),
        Some(bytes) => decode_saved(&bytes).map_err(|e| e.to_string()),
    }
}

/// Undo a save or delete the server refused, so the menu shows what is
/// stored. Only while the list is still the one that failed: a later
/// change has its own outcome.
/// What a mix that could not be saved says (B824).
fn not_saved_line(name: &str, reason: &str) -> String {
    format!("Couldn't save the mix \"{name}\": {reason}")
}

/// What a mix that could not be deleted says (B824).
fn not_deleted_line(name: &str, reason: &str) -> String {
    format!("Couldn't delete the mix \"{name}\": {reason}")
}

fn roll_back(mut saved: Signal<Option<Vec<SavedMix>>>, failed: &[SavedMix], before: Vec<SavedMix>) {
    if saved.peek().as_deref() == Some(failed) {
        saved.set(Some(before));
    }
}

async fn store_saved(api: &NativeApi, mixes: &[SavedMix]) -> Result<(), String> {
    api.set_pref(
        &call("listen", "mix_saved_write"),
        SAVED_KEY,
        &encode_saved(mixes),
    )
    .await
    .map_err(|e| e.to_string())
}

/// `m` as it can be stored in `list`: a name a newer app's mix holds gets
/// a number, so saving never overwrites rules this app cannot read.
pub fn saved_name(list: &[SavedMix], mut m: SavedMix) -> SavedMix {
    let newer = |name: &str| list.iter().any(|s| s.name == name && s.newer.is_some());
    if newer(&m.name) {
        let base = m.name.clone();
        let mut n = 2;
        while newer(&format!("{base} ({n})")) {
            n += 1;
        }
        m.name = format!("{base} ({n})");
    }
    m
}

/// What the mix line in the queue panel says under the mix's name.
pub fn status_text(mix: &ActiveMix) -> Option<String> {
    if let Some(e) = &mix.error {
        Some(format!("Paused: {e}"))
    } else if mix.exhausted {
        Some("This mix has run out of songs.".to_owned())
    } else if mix.asking {
        Some("Finding songs…".to_owned())
    } else {
        None
    }
}

/// The mix line at the top of the queue panel: the mix that is on with
/// Edit and Stop, or a Mix button that opens the picker.
#[component]
pub fn MixBar(playing: Option<ItemRef>) -> Element {
    // Every hook runs before the early return, so the hook order never
    // depends on which contexts are present.
    let ctx = try_use_context::<Ctx>();
    let flash = try_use_context::<Flash>();
    let say = move |line: String| {
        if let Some(flash) = flash {
            flash.say(line, true);
        }
    };
    let mut open = use_signal(|| false);
    let mut editing = use_signal(|| None::<SavedMix>);
    // Counts each opening of the editor; the editor remounts on it, so a
    // new draft gets a fresh editor rather than the last one's edits.
    let mut edits = use_signal(|| 0u64);
    let mut saved = use_signal(|| None::<Vec<SavedMix>>);
    let mut load_error = use_signal(|| None::<String>);
    // `playing` is None in a SyncPlay group, where the group picks the
    // songs: no mix there.
    let (Some(MixCtx(mut mix)), Some(QueueCtx(mut queue)), Some(_)) = (
        try_use_context::<MixCtx>(),
        try_use_context::<QueueCtx>(),
        playing,
    ) else {
        return rsx! {};
    };
    let load = move || {
        if saved.peek().is_some() {
            return;
        }
        let Some(api) = ctx.and_then(|c| c.api.peek().clone()) else {
            return;
        };
        spawn(async move {
            match load_saved(&api).await {
                Ok(list) => {
                    load_error.set(None);
                    saved.set(Some(list));
                }
                // Never an empty list: a save would then wipe the real one.
                Err(e) => {
                    platform::warn(&format!("pharos: mix saved list unavailable error={e}"));
                    load_error.set(Some(e));
                }
            }
        });
    };
    let mut begin = move |name: String, spec: MixSpec| {
        open.set(false);
        // The player is open, so a song plays: the mix follows it.
        start(&mut queue.write(), &mut mix.write(), name, spec, false);
    };
    let mut on_save = move |m: SavedMix| {
        let Some(api) = ctx.and_then(|c| c.api.peek().clone()) else {
            return;
        };
        // Only over a list that loaded: writing over one that did not
        // would replace every mix the user saved.
        let Some(mut list) = saved.peek().clone() else {
            platform::warn(&format!(
                "pharos: mix not saved mix={:?} reason=list_not_loaded",
                m.name
            ));
            say(not_saved_line(
                &m.name,
                "your saved mixes have not loaded yet",
            ));
            return;
        };
        let before = list.clone();
        let m = saved_name(&list, m);
        match list.iter_mut().find(|s| s.name == m.name) {
            Some(slot) => *slot = m.clone(),
            None => list.push(m.clone()),
        }
        saved.set(Some(list.clone()));
        spawn(async move {
            match store_saved(&api, &list).await {
                Ok(()) => platform::info(&format!(
                    "pharos: mix saved mix={:?} total={}",
                    m.name,
                    list.len()
                )),
                Err(e) => {
                    platform::warn(&format!("pharos: mix not saved mix={:?} error={e}", m.name));
                    roll_back(saved, &list, before);
                    say(not_saved_line(&m.name, &e));
                }
            }
        });
    };
    let mut on_delete = move |name: String| {
        let Some(api) = ctx.and_then(|c| c.api.peek().clone()) else {
            return;
        };
        let Some(mut list) = saved.peek().clone() else {
            platform::warn(&format!(
                "pharos: mix not deleted mix={name:?} reason=list_not_loaded"
            ));
            say(not_deleted_line(
                &name,
                "your saved mixes have not loaded yet",
            ));
            return;
        };
        let before = list.clone();
        list.retain(|s| s.name != name);
        saved.set(Some(list.clone()));
        spawn(async move {
            if let Err(e) = store_saved(&api, &list).await {
                platform::warn(&format!("pharos: mix not deleted mix={name:?} error={e}"));
                roll_back(saved, &list, before);
                say(not_deleted_line(&name, &e));
            }
        });
    };
    let active = mix.read().clone();
    rsx! {
        div { class: "pharos-mix-bar",
            if let Some(m) = active.clone() {
                div { class: "pharos-mix-on", role: "status",
                    span { class: "pharos-mix-name", "Mix: {m.name}" }
                    if let Some(text) = status_text(&m) {
                        span { class: "pharos-muted pharos-mix-status", "{text}" }
                    }
                }
                if m.error.is_some() {
                    button {
                        class: "pharos-chip",
                        r#type: "button",
                        onclick: move |_| {
                            if let Some(m) = mix.write().as_mut() {
                                m.error = None;
                            }
                        },
                        "Retry"
                    }
                }
                button {
                    class: "pharos-chip",
                    r#type: "button",
                    onclick: move |_| {
                        load();
                        let m = mix.peek().clone();
                        if let Some(m) = m {
                            *edits.write() += 1;
                            editing.set(Some(SavedMix::new(m.name, m.spec)));
                        }
                    },
                    "Edit"
                }
                button {
                    class: "pharos-chip pharos-mix-stop",
                    r#type: "button",
                    onclick: move |_| stop(&mut queue.write(), &mut mix.write()),
                    "Stop mix"
                }
            } else {
                button {
                    class: "pharos-chip pharos-mix-toggle",
                    "data-key-action": "mix",
                    r#type: "button",
                    aria_expanded: if open() { "true" } else { "false" },
                    onclick: move |_| {
                        let now = !open();
                        open.set(now);
                        if now {
                            load();
                        }
                    },
                    Icon { path: ICON_MIX }
                    "Mix"
                }
            }
        }
        if open() && active.is_none() {
            ul { class: "pharos-mix-menu", aria_label: "Start a mix",
                if let Some(song) = playing {
                    li {
                        button {
                            class: "pharos-mix-choice",
                            r#type: "button",
                            onclick: move |_| begin("More like this".to_owned(), radio(song)),
                            "More like this song"
                        }
                    }
                }
                for preset in presets() {
                    li { key: "preset-{preset.name}",
                        button {
                            class: "pharos-mix-choice",
                            r#type: "button",
                            onclick: {
                                let preset = preset.clone();
                                move |_| begin(preset.name.clone(), preset.spec.clone())
                            },
                            "{preset.name}"
                        }
                    }
                }
                for mine in saved().unwrap_or_default() {
                    li { key: "saved-{mine.name}", class: "pharos-mix-saved",
                        // A newer app's mix holds rules this one cannot
                        // read: playing it would skip them, editing would
                        // drop them. It can still be deleted.
                        button {
                            class: "pharos-mix-choice",
                            r#type: "button",
                            disabled: mine.newer.is_some(),
                            title: if mine.newer.is_some() { "Made with a newer Pharos app" } else { "" },
                            onclick: {
                                let mine = mine.clone();
                                move |_| begin(mine.name.clone(), mine.spec.clone())
                            },
                            "{mine.name}"
                        }
                        button {
                            class: "pharos-queue-icon",
                            r#type: "button",
                            disabled: mine.newer.is_some(),
                            aria_label: "Edit {mine.name}",
                            title: "Edit",
                            onclick: {
                                let mine = mine.clone();
                                move |_| {
                                    *edits.write() += 1;
                                    editing.set(Some(mine.clone()));
                                }
                            },
                            "✎"
                        }
                        button {
                            class: "pharos-queue-icon",
                            r#type: "button",
                            aria_label: "Delete {mine.name}",
                            title: "Delete",
                            onclick: {
                                let name = mine.name.clone();
                                move |_| on_delete(name.clone())
                            },
                            "×"
                        }
                    }
                }
                li {
                    button {
                        class: "pharos-mix-choice pharos-mix-new",
                        r#type: "button",
                        onclick: move |_| {
                            *edits.write() += 1;
                            editing.set(Some(SavedMix::new(
                                "My mix",
                                MixSpec { artist_gap: 3, album_gap: 5, ..MixSpec::default() },
                            )));
                        },
                        "New mix…"
                    }
                }
            }
        }
        if let Some(draft) = editing() {
            {remount_on(&edits().to_string(), rsx! {
                MixEditor {
                    draft,
                    playing,
                    can_save: saved().is_some(),
                    save_note: load_error(),
                    on_close: move |_| editing.set(None),
                    on_save: move |m: SavedMix| {
                        on_save(m);
                        editing.set(None);
                    },
                    on_play: move |m: SavedMix| {
                        editing.set(None);
                        begin(m.name, m.spec);
                    },
                }
            })}
        }
    }
}

/// Where a mix on a music library's Playlists tab comes from (B897/V846).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShelfKind {
    /// One every first-party client offers.
    Preset,
    /// One the listener saved.
    Saved,
}

impl ShelfKind {
    /// The log and test label; stable.
    pub fn label(self) -> &'static str {
        match self {
            ShelfKind::Preset => "preset",
            ShelfKind::Saved => "saved",
        }
    }
}

/// The mixes the Playlists tab offers, in order: the presets, then the
/// listener's own as saved.
pub fn shelf(saved: &[SavedMix]) -> Vec<(ShelfKind, SavedMix)> {
    presets()
        .into_iter()
        .map(|m| (ShelfKind::Preset, m))
        .chain(saved.iter().cloned().map(|m| (ShelfKind::Saved, m)))
        .collect()
}

/// One rule as a tile says it, value included.
fn rule_text(r: &Rule) -> String {
    let what = match &r.test {
        Test::Genre(s) | Test::Artist(s) | Test::Album(s) => s.clone(),
        Test::Years { from, to } => format!("{from}–{to}"),
        Test::Favorite => "favourites".to_owned(),
        Test::NeverPlayed => "never played".to_owned(),
        Test::PlayCount { min, max } if *max == u32::MAX => format!("played {min}+ times"),
        Test::PlayCount { min, max } => format!("played {min}–{max} times"),
        Test::NotPlayedForDays(d) => format!("not played for {d} days"),
        Test::PlayedWithinDays(d) => format!("played in the last {d} days"),
        Test::AddedWithinDays(d) => format!("added in the last {d} days"),
        Test::LengthSecs { min, max } => format!("{}–{} min long", min / 60, max / 60),
        Test::LikeSeed => "like the last song".to_owned(),
        Test::Tempo { min, max } => format!("{min}–{max} BPM"),
        Test::Energy { min, max } => format!("energy {min}–{max}%"),
    };
    format!("{} {what}", r.weight.label())
}

/// What a mix picks, in a line: its rules in order, or any song.
pub fn describe(spec: &MixSpec) -> String {
    if spec.rules.is_empty() {
        return "Any song".to_owned();
    }
    spec.rules
        .iter()
        .map(rule_text)
        .collect::<Vec<_>>()
        .join(" · ")
}

/// A music library's Playlists tab (B897/V846): every mix the listener can
/// start, presets first, then their saved ones. A tile starts its mix and
/// opens the player on the first song, so a mix is picked before the
/// player, not only from its queue panel.
#[component]
pub fn MixShelf() -> Element {
    let ctx = try_use_context::<Ctx>();
    let together = try_use_context::<Together>();
    let mix_ctx = try_use_context::<MixCtx>();
    let queue_ctx = try_use_context::<QueueCtx>();
    let mut saved = use_signal(|| None::<Result<Vec<SavedMix>, String>>);
    let mut attempt = use_signal(|| 0u32);
    use_effect(move || {
        let tries = attempt();
        // Read, not peeked: the list loads once the API connects.
        let Some(api) = ctx.and_then(|c| c.api.read().clone()) else {
            return;
        };
        spawn(async move {
            let got = load_saved(&api).await;
            match &got {
                Ok(list) => platform::info(&format!(
                    "pharos: mix shelf loaded saved={} attempt={tries}",
                    list.len()
                )),
                Err(e) => platform::warn(&format!(
                    "pharos: mix shelf saved list unavailable attempt={tries} error={e}"
                )),
            }
            saved.set(Some(got));
        });
    });
    let in_group = together.as_ref().is_some_and(|t| t.view.read().active);
    let active = mix_ctx.and_then(|MixCtx(m)| m.read().clone());
    let state = saved();
    let mine = state.clone().and_then(Result::ok).unwrap_or_default();
    let tiles = shelf(&mine);
    rsx! {
        section { class: "pharos-mix-shelf", aria_label: "Playlists",
            if in_group {
                p { class: "pharos-muted pharos-mix-shelf-note",
                    "In a Together group the group picks the songs, so mixes are off."
                }
            }
            ul { class: "pharos-mix-tiles",
                for (kind, m) in tiles {
                    li {
                        key: "{kind.label()}-{m.name}",
                        class: "pharos-mix-tile",
                        "data-kind": kind.label(),
                        {
                            let on = active.as_ref().filter(|a| a.name == m.name);
                            let status = on.and_then(status_text);
                            let newer = m.newer.is_some();
                            let name = m.name.clone();
                            let spec = m.spec.clone();
                            rsx! {
                                button {
                                    class: "pharos-mix-tile-play",
                                    r#type: "button",
                                    "data-key-action": "play",
                                    disabled: in_group || newer,
                                    title: if newer { "Made with a newer Pharos app" } else { "" },
                                    aria_label: "Play the mix {m.name}",
                                    onclick: move |_| {
                                        let (Some(MixCtx(mut mix)), Some(QueueCtx(mut queue))) = (mix_ctx, queue_ctx) else {
                                            platform::warn(&format!(
                                                "pharos: mix shelf start refused mix={name:?} reason=no_queue"
                                            ));
                                            return;
                                        };
                                        platform::info(&format!(
                                            "pharos: mix shelf start mix={name:?} kind={}",
                                            kind.label()
                                        ));
                                        start(&mut queue.write(), &mut mix.write(), name.clone(), spec.clone(), true);
                                    },
                                    span { class: "pharos-mix-tile-art", aria_hidden: "true",
                                        Icon { path: ICON_MIX }
                                    }
                                    span { class: "pharos-mix-tile-name", "{m.name}" }
                                    span { class: "pharos-muted pharos-mix-tile-rules", {describe(&m.spec)} }
                                    if on.is_some() {
                                        span { class: "pharos-mix-tile-on", role: "status",
                                            {status.unwrap_or_else(|| "Playing".to_owned())}
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            match state {
                None => rsx! {
                    p { class: "pharos-muted pharos-mix-shelf-note", "Loading your mixes…" }
                },
                Some(Err(e)) => rsx! {
                    div { class: "pharos-mix-shelf-note", role: "alert",
                        p { "Couldn't load your saved mixes: {e}" }
                        button {
                            class: "pharos-chip",
                            r#type: "button",
                            onclick: move |_| {
                                saved.set(None);
                                *attempt.write() += 1;
                            },
                            "Try again"
                        }
                    }
                },
                Some(Ok(list)) if list.is_empty() => rsx! {
                    p { class: "pharos-muted pharos-mix-shelf-note",
                        "Mixes you make with New mix in the player's queue show here too."
                    }
                },
                Some(Ok(_)) => rsx! {},
            }
        }
    }
}

/// A rule's value, as the editor's text fields hold it.
fn rule_value(test: &Test) -> (String, String) {
    match test {
        Test::Genre(s) | Test::Artist(s) | Test::Album(s) => (s.clone(), String::new()),
        Test::Years { from, to } => (from.to_string(), to.to_string()),
        Test::PlayCount { min, max } => (
            min.to_string(),
            if *max == u32::MAX {
                String::new()
            } else {
                max.to_string()
            },
        ),
        Test::LengthSecs { min, max } => ((min / 60).to_string(), (max / 60).to_string()),
        Test::NotPlayedForDays(d) | Test::PlayedWithinDays(d) | Test::AddedWithinDays(d) => {
            (d.to_string(), String::new())
        }
        Test::Tempo { min, max } => (min.to_string(), max.to_string()),
        Test::Energy { min, max } => (min.to_string(), max.to_string()),
        Test::Favorite | Test::NeverPlayed | Test::LikeSeed => (String::new(), String::new()),
    }
}

/// `test` with its value set from the editor's fields; an unreadable number
/// keeps the old one.
pub fn with_value(test: &Test, a: &str, b: &str) -> Test {
    let num = |s: &str, old: u32| s.trim().parse::<u32>().unwrap_or(old);
    match test {
        Test::Genre(_) => Test::Genre(a.to_owned()),
        Test::Artist(_) => Test::Artist(a.to_owned()),
        Test::Album(_) => Test::Album(a.to_owned()),
        Test::Years { from, to } => Test::Years {
            from: a.trim().parse().unwrap_or(*from),
            to: b.trim().parse().unwrap_or(*to),
        },
        Test::PlayCount { min, max } => Test::PlayCount {
            min: num(a, *min),
            max: if b.trim().is_empty() {
                u32::MAX
            } else {
                num(b, *max)
            },
        },
        Test::LengthSecs { min, max } => Test::LengthSecs {
            min: num(a, min / 60).saturating_mul(60),
            max: num(b, max / 60).saturating_mul(60),
        },
        Test::NotPlayedForDays(d) => Test::NotPlayedForDays(num(a, *d)),
        Test::PlayedWithinDays(d) => Test::PlayedWithinDays(num(a, *d)),
        Test::AddedWithinDays(d) => Test::AddedWithinDays(num(a, *d)),
        Test::Tempo { min, max } => Test::Tempo {
            min: a.trim().parse().unwrap_or(*min),
            max: b.trim().parse().unwrap_or(*max),
        },
        Test::Energy { min, max } => Test::Energy {
            min: a.trim().parse::<u8>().map_or(*min, |v| v.min(100)),
            max: b.trim().parse::<u8>().map_or(*max, |v| v.min(100)),
        },
        Test::Favorite | Test::NeverPlayed | Test::LikeSeed => test.clone(),
    }
}

/// What the editor shows beside a value field, so "30" reads "30 days".
fn units(test: &Test) -> (&'static str, &'static str) {
    match test {
        Test::Years { .. } => ("from", "to"),
        Test::PlayCount { .. } => ("from", "to (blank: any)"),
        Test::LengthSecs { .. } => ("from (min)", "to (min)"),
        Test::Tempo { .. } => ("from (BPM)", "to (BPM)"),
        Test::Energy { .. } => ("from (%, calm)", "to (%, intense)"),
        Test::NotPlayedForDays(_) | Test::PlayedWithinDays(_) | Test::AddedWithinDays(_) => {
            ("days", "")
        }
        _ => ("", ""),
    }
}

fn takes_two(test: &Test) -> bool {
    matches!(
        test,
        Test::Years { .. }
            | Test::PlayCount { .. }
            | Test::LengthSecs { .. }
            | Test::Tempo { .. }
            | Test::Energy { .. }
    )
}

fn takes_one(test: &Test) -> bool {
    !matches!(test, Test::Favorite | Test::NeverPlayed | Test::LikeSeed)
}

/// Why `draft` cannot be played or saved yet, in words; `None` when it can.
/// The server checks again; this catches what the editor can see.
pub fn draft_problem(draft: &SavedMix) -> Option<String> {
    if draft.name.trim().is_empty() {
        return Some("Give the mix a name.".to_owned());
    }
    for (i, r) in draft.spec.rules.iter().enumerate() {
        let n = i + 1;
        match &r.test {
            Test::Genre(s) | Test::Artist(s) | Test::Album(s) if s.trim().is_empty() => {
                return Some(format!(
                    "Rule {n}: fill in the {}.",
                    r.test.kind_label().to_lowercase()
                ));
            }
            Test::Years { from, to } if to < from => {
                return Some(format!("Rule {n}: the years end before they start."));
            }
            Test::PlayCount { min, max } | Test::LengthSecs { min, max } if max < min => {
                return Some(format!("Rule {n}: the range ends before it starts."));
            }
            Test::Tempo { min, max } if max < min => {
                return Some(format!("Rule {n}: the tempo range ends before it starts."));
            }
            Test::Energy { min, max } if max < min => {
                return Some(format!("Rule {n}: the energy range ends before it starts."));
            }
            Test::LikeSeed if draft.spec.seed == Seed::None => {
                return Some(format!("Rule {n}: pick what the mix should be like."));
            }
            _ => {}
        }
        if r.share > 100 {
            return Some(format!("Rule {n}: a share is at most 100%."));
        }
    }
    None
}

#[component]
fn MixEditor(
    draft: SavedMix,
    playing: Option<ItemRef>,
    /// The saved list has loaded, so a save cannot lose it.
    can_save: bool,
    /// Why it has not, when it failed.
    save_note: Option<String>,
    on_close: EventHandler<()>,
    on_save: EventHandler<SavedMix>,
    on_play: EventHandler<SavedMix>,
) -> Element {
    let mut d = use_signal(|| draft.clone());
    let problem = draft_problem(&d.read());
    let seed_value = seed_label(d.read().spec.seed);
    let rules = d.read().spec.rules.clone();
    rsx! {
        div {
            class: "pharos-mix-editor",
            role: "dialog",
            aria_modal: "true",
            aria_label: "Edit mix",
            tabindex: "-1",
            // Focus moves into the dialog, so Escape closes it straight away.
            onmounted: move |e: MountedEvent| async move {
                let _ = e.set_focus(true).await;
            },
            onkeydown: move |e: KeyboardEvent| {
                e.stop_propagation();
                if e.key() == Key::Named(NamedKey::Escape) {
                    on_close.call(());
                }
            },
            // Mouse input in the dialog must not reach the player's
            // Shift-click shortcuts, which skip tracks.
            onclick: move |e: MouseEvent| e.stop_propagation(),
            onmousedown: move |e: MouseEvent| e.stop_propagation(),
            oncontextmenu: move |e: MouseEvent| e.stop_propagation(),
            label { class: "pharos-mix-field",
                span { "Name" }
                input {
                    r#type: "text",
                    value: "{d.read().name}",
                    oninput: move |e| d.write().name = e.value(),
                }
            }
            ol { class: "pharos-mix-rules",
                for (i, rule) in rules.into_iter().enumerate() {
                    li { key: "{i}", class: "pharos-mix-rule",
                        select {
                            aria_label: "Rule {i + 1} strength",
                            onchange: move |e: FormEvent| {
                                let w = Weight::ALL.into_iter().find(|w| w.label() == e.value());
                                if let (Some(w), Some(r)) = (w, d.write().spec.rules.get_mut(i)) {
                                    r.weight = w;
                                    if w != Weight::Prefer {
                                        r.share = 0;
                                    }
                                }
                            },
                            for w in Weight::ALL {
                                option { value: w.label(), selected: w == rule.weight, "{w.label()}" }
                            }
                        }
                        select {
                            aria_label: "Rule {i + 1} test",
                            onchange: move |e: FormEvent| {
                                let t = Test::kinds().into_iter().find(|t| t.kind_label() == e.value());
                                if let (Some(t), Some(r)) = (t, d.write().spec.rules.get_mut(i)) {
                                    r.test = t;
                                }
                            },
                            for t in Test::kinds() {
                                option {
                                    value: t.kind_label(),
                                    selected: t.kind_label() == rule.test.kind_label(),
                                    "{t.kind_label()}"
                                }
                            }
                        }
                        RuleValue { index: i, test: rule.test.clone(), draft: d }
                        if rule.weight == Weight::Prefer {
                            label { class: "pharos-mix-share",
                                span { "about" }
                                input {
                                    r#type: "number",
                                    min: "0",
                                    max: "100",
                                    aria_label: "Rule {i + 1} share, percent (0 for no share)",
                                    value: "{rule.share}",
                                    oninput: move |e| {
                                        if let (Ok(v), Some(r)) = (e.value().trim().parse::<u8>(), d.write().spec.rules.get_mut(i)) {
                                            r.share = v.min(100);
                                        }
                                    },
                                }
                                span { "% of songs" }
                            }
                        }
                        button {
                            class: "pharos-queue-icon",
                            r#type: "button",
                            aria_label: "Remove rule {i + 1}",
                            onclick: move |_| {
                                let mut w = d.write();
                                if i < w.spec.rules.len() {
                                    w.spec.rules.remove(i);
                                }
                            },
                            "×"
                        }
                    }
                }
            }
            button {
                class: "pharos-chip",
                r#type: "button",
                onclick: move |_| d.write().spec.rules.push(Rule {
                    weight: Weight::Require,
                    share: 0,
                    test: Test::Genre(String::new()),
                }),
                "Add rule"
            }
            label { class: "pharos-mix-field",
                span { "Like" }
                select {
                    onchange: move |e: FormEvent| {
                        let kept = d.peek().spec.seed;
                        d.write().spec.seed = match (e.value().as_str(), playing, kept) {
                            ("follow", _, _) => Seed::Follow,
                            ("song", Some(song), _) => Seed::Song(song),
                            // Nothing playing: keep the song it was like.
                            ("song", None, Seed::Song(song)) => Seed::Song(song),
                            _ => Seed::None,
                        };
                    },
                    option { value: "none", selected: seed_value == "none", "Nothing in particular" }
                    option { value: "follow", selected: seed_value == "follow", "Whatever played last" }
                    if playing.is_some() || seed_value == "song" {
                        option { value: "song", selected: seed_value == "song", "This song" }
                    }
                }
            }
            label { class: "pharos-mix-field",
                span { "No artist again within" }
                input {
                    r#type: "number",
                    min: "0",
                    max: "20",
                    value: "{d.read().spec.artist_gap}",
                    oninput: move |e| {
                        if let Ok(v) = e.value().trim().parse::<u8>() {
                            d.write().spec.artist_gap = v.min(20);
                        }
                    },
                }
                span { "songs" }
            }
            label { class: "pharos-mix-field",
                span { "No album again within" }
                input {
                    r#type: "number",
                    min: "0",
                    max: "20",
                    value: "{d.read().spec.album_gap}",
                    oninput: move |e| {
                        if let Ok(v) = e.value().trim().parse::<u8>() {
                            d.write().spec.album_gap = v.min(20);
                        }
                    },
                }
                span { "songs" }
            }
            if let Some(note) = save_note {
                p { class: "pharos-mix-problem", role: "status",
                    "Saved mixes could not be loaded, so this one cannot be saved yet: {note}"
                }
            }
            if let Some(p) = problem.clone() {
                p { class: "pharos-mix-problem", role: "alert", "{p}" }
            }
            div { class: "pharos-mix-actions",
                button {
                    class: "pharos-chip",
                    r#type: "button",
                    disabled: problem.is_some(),
                    onclick: move |_| on_play.call(d.peek().clone()),
                    "Play"
                }
                button {
                    class: "pharos-chip",
                    r#type: "button",
                    disabled: problem.is_some() || !can_save,
                    title: if can_save { "" } else { "Your saved mixes have not loaded yet" },
                    onclick: move |_| on_save.call(d.peek().clone()),
                    "Save"
                }
                button {
                    class: "pharos-chip",
                    r#type: "button",
                    onclick: move |_| on_close.call(()),
                    "Cancel"
                }
            }
        }
    }
}

#[component]
fn RuleValue(index: usize, test: Test, draft: Signal<SavedMix>) -> Element {
    let (a, b) = rule_value(&test);
    let (ua, ub) = units(&test);
    let one = takes_one(&test);
    let two = takes_two(&test);
    let n = index + 1;
    let mut set = move |first: Option<String>, second: Option<String>| {
        let mut w = draft.write();
        if let Some(r) = w.spec.rules.get_mut(index) {
            let (a, b) = rule_value(&r.test);
            r.test = with_value(&r.test, &first.unwrap_or(a), &second.unwrap_or(b));
        }
    };
    rsx! {
        if one {
            input {
                r#type: if matches!(test, Test::Genre(_) | Test::Artist(_) | Test::Album(_)) { "text" } else { "number" },
                aria_label: "Rule {n} value",
                value: "{a}",
                oninput: move |e| set(Some(e.value()), None),
            }
            if !ua.is_empty() { span { class: "pharos-muted", "{ua}" } }
        }
        if two {
            input {
                r#type: "number",
                aria_label: "Rule {n} upper value",
                value: "{b}",
                oninput: move |e| set(None, Some(e.value())),
            }
            if !ub.is_empty() { span { class: "pharos-muted", "{ub}" } }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// B824: a mix that was not stored says which and why.
    #[test]
    fn a_mix_that_was_not_stored_names_itself_and_the_reason() {
        assert_eq!(
            not_saved_line("Late night", "unavailable: db down"),
            "Couldn't save the mix \"Late night\": unavailable: db down"
        );
        assert_eq!(
            not_deleted_line("Late night", "rpc: gone"),
            "Couldn't delete the mix \"Late night\": rpc: gone"
        );
    }
    use pharos_client::api::{Item, ItemKind};

    fn item(id: u64) -> Item {
        Item {
            id: ItemRef::Real(id),
            kind: ItemKind::Audio,
            name: format!("Song {id}"),
            year: None,
            runtime_ms: 1000,
            image: None,
            played: false,
            favorite: false,
            position_ms: 0,
            series_name: None,
            season: None,
            episode: None,
            parent: None,
            library_kind: None,
            imdb_rating: None,
            critic_rating: None,
            artist: None,
        }
    }

    fn track(id: u64) -> Track {
        Track::from(&item(id))
    }

    fn on(spec: MixSpec) -> ActiveMix {
        ActiveMix {
            name: "Test".into(),
            spec,
            generation: 1,
            asking: false,
            exhausted: false,
            error: None,
            play_first: false,
            reasons: HashMap::new(),
        }
    }

    #[test]
    fn the_mix_asks_only_when_the_queue_runs_low_and_it_can() {
        let mut q = Queue::default();
        let mut m = on(MixSpec::default());
        assert!(needs_more(&q, &m));
        q.append((0..LOW_WATER as u64).map(track).collect(), Source::Mix);
        assert!(!needs_more(&q, &m), "a full queue waits");
        q.drop_source(Source::Mix);
        m.asking = true;
        assert!(!needs_more(&q, &m), "one ask at a time");
        m.asking = false;
        m.exhausted = true;
        assert!(!needs_more(&q, &m), "a mix that ran out stops asking");
        m.exhausted = false;
        m.error = Some("down".into());
        assert!(!needs_more(&q, &m), "a failed ask waits for Retry");
    }

    #[test]
    fn a_mix_that_plays_now_asks_even_behind_a_long_queue() {
        let mut q = Queue::default();
        q.add((0..LOW_WATER as u64 + 3).map(track).collect());
        let mut m = on(MixSpec::default());
        assert!(!needs_more(&q, &m), "a full queue waits");
        m.play_first = true;
        assert!(needs_more(&q, &m), "Start a mix like plays at once");
        m.asking = true;
        assert!(!needs_more(&q, &m), "still one ask at a time");
    }

    #[test]
    fn history_is_oldest_first_then_what_is_queued() {
        let mut q = Queue::default();
        q.started(ItemRef::Real(1), None, 0);
        q.started(ItemRef::Real(2), None, 0);
        q.append(vec![track(3), track(4)], Source::Mix);
        assert_eq!(history(&q), [1, 2, 3, 4].map(ItemRef::Real).to_vec());
    }

    #[test]
    fn starting_a_mix_keeps_what_the_listener_added_and_drops_the_rest() {
        let mut q = Queue::default();
        q.add(vec![track(1)]);
        q.append(vec![track(2)], Source::Mix);
        q.append(vec![track(3)], Source::Album);
        let mut m = None;
        start(&mut q, &mut m, "A".into(), MixSpec::default(), false);
        let ids: Vec<ItemRef> = q.upcoming().iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![ItemRef::Real(1)]);
        let first = m.as_ref().map(|m| m.generation).unwrap();
        start(&mut q, &mut m, "B".into(), MixSpec::default(), true);
        let m2 = m.clone().unwrap();
        // The counter is shared by every test running in parallel, so only
        // its order is certain.
        assert!(
            m2.generation > first,
            "a restart outdates answers in flight"
        );
        assert!(m2.play_first);
        q.append(vec![track(5)], Source::Mix);
        stop(&mut q, &mut m);
        assert!(m.is_none());
        assert_eq!(q.upcoming().len(), 1, "stop takes out only the mix's songs");
    }

    #[test]
    fn a_song_played_at_once_goes_to_the_front_so_it_leaves_the_queue() {
        let mut q = Queue::default();
        q.add(vec![track(1)]);
        apply_batch(&mut q, vec![track(5), track(6)], Some(ItemRef::Real(5)));
        let ids: Vec<ItemRef> = q.upcoming().iter().map(|e| e.id).collect();
        assert_eq!(ids, [5, 1, 6].map(ItemRef::Real).to_vec());
        q.started(ItemRef::Real(5), Some(&[]), 0);
        assert!(!q.contains(ItemRef::Real(5)), "it does not play twice");
        let mut q = Queue::default();
        q.add(vec![track(1)]);
        apply_batch(&mut q, vec![track(5)], None);
        assert_eq!(q.upcoming().last().map(|e| e.id), Some(ItemRef::Real(5)));
    }

    #[test]
    fn a_song_started_during_a_mix_queues_none_of_its_album() {
        let album = [item(2), item(3)];
        let mut q = Queue::default();
        q.started(ItemRef::Real(1), album_rest(true, Some(&album)), 0);
        assert!(q.upcoming().is_empty(), "the mix decides what follows");
        let mut q = Queue::default();
        q.started(ItemRef::Real(1), album_rest(false, Some(&album)), 0);
        assert_eq!(q.upcoming().len(), 2, "without a mix the album follows");
        assert_eq!(album_rest::<Item>(true, None), None);
    }

    #[test]
    fn generations_never_repeat_even_after_a_stop() {
        let mut q = Queue::default();
        let mut m = None;
        start(&mut q, &mut m, "A".into(), MixSpec::default(), false);
        let a = m.as_ref().unwrap().generation;
        stop(&mut q, &mut m);
        start(&mut q, &mut m, "B".into(), MixSpec::default(), false);
        assert!(m.as_ref().unwrap().generation > a);
    }

    #[test]
    fn saving_keeps_the_name_when_no_newer_mix_holds_it() {
        let list = vec![SavedMix::new("Gym", MixSpec::default())];
        // A plain mix of the same name is replaced in place, not renamed.
        assert_eq!(
            saved_name(&list, SavedMix::new("Gym", MixSpec::default())).name,
            "Gym"
        );
        assert_eq!(
            saved_name(&list, SavedMix::new("Run", MixSpec::default())).name,
            "Run"
        );
    }

    #[test]
    fn the_status_line_says_why_the_mix_is_quiet() {
        let mut m = on(MixSpec::default());
        assert_eq!(status_text(&m), None);
        m.asking = true;
        assert_eq!(status_text(&m).as_deref(), Some("Finding songs…"));
        m.exhausted = true;
        assert_eq!(
            status_text(&m).as_deref(),
            Some("This mix has run out of songs.")
        );
        m.error = Some("media store".into());
        assert_eq!(status_text(&m).as_deref(), Some("Paused: media store"));
    }

    #[test]
    fn the_top_up_line_carries_what_the_server_decided() {
        let batch = MixBatch {
            picks: vec![],
            pool: 10,
            eligible: 3,
            exhausted: true,
            relaxed: vec!["spacing".into(), "repeat".into()],
        };
        assert_eq!(
            top_up_line("Drift", 10, &batch),
            "pharos: mix top-up mix=\"Drift\" asked=10 got=0 exhausted=true relaxed=spacing,repeat eligible=3"
        );
    }

    #[test]
    fn editor_values_round_trip_and_bad_numbers_keep_the_old_value() {
        for t in Test::kinds() {
            let (a, b) = rule_value(&t);
            assert_eq!(with_value(&t, &a, &b), t, "{t:?}");
        }
        let years = Test::Years {
            from: 1990,
            to: 1999,
        };
        assert_eq!(
            with_value(&years, "x", "2005"),
            Test::Years {
                from: 1990,
                to: 2005
            }
        );
        let count = Test::PlayCount { min: 1, max: 4 };
        assert_eq!(
            with_value(&count, "2", ""),
            Test::PlayCount {
                min: 2,
                max: u32::MAX
            }
        );
        let energy = Test::Energy { min: 10, max: 50 };
        assert_eq!(
            with_value(&energy, "20", "300"),
            Test::Energy { min: 20, max: 50 },
            "300 is no u8 percent"
        );
        assert_eq!(
            with_value(&energy, "20", "180"),
            Test::Energy { min: 20, max: 100 }
        );
        let length = Test::LengthSecs { min: 120, max: 360 };
        assert_eq!(
            with_value(&length, "3", "8"),
            Test::LengthSecs { min: 180, max: 480 }
        );
    }

    #[test]
    fn a_draft_names_its_first_problem() {
        let mut d = SavedMix::new(" ", MixSpec::default());
        assert_eq!(draft_problem(&d).as_deref(), Some("Give the mix a name."));
        d.name = "Mine".into();
        assert_eq!(draft_problem(&d), None);
        d.spec.rules.push(Rule {
            weight: Weight::Require,
            share: 0,
            test: Test::Genre(String::new()),
        });
        assert_eq!(
            draft_problem(&d).as_deref(),
            Some("Rule 1: fill in the genre.")
        );
        d.spec.rules[0].test = Test::LikeSeed;
        assert_eq!(
            draft_problem(&d).as_deref(),
            Some("Rule 1: pick what the mix should be like.")
        );
        d.spec.seed = Seed::Follow;
        assert_eq!(draft_problem(&d), None);
    }

    #[test]
    fn there_is_no_mix_bar_in_a_group() {
        let mut dom = VirtualDom::new(|| {
            use_context_provider(|| QueueCtx(Signal::new(Queue::default())));
            use_context_provider(|| MixCtx(Signal::new(None)));
            rsx! { MixBar { playing: None } }
        });
        dom.rebuild_in_place();
        assert!(!dioxus_ssr::render(&dom).contains("pharos-mix"));
    }

    #[test]
    fn the_mix_bar_offers_a_mix_and_more_like_this_song() {
        let mut dom = VirtualDom::new(|| {
            use_context_provider(|| QueueCtx(Signal::new(Queue::default())));
            use_context_provider(|| MixCtx(Signal::new(None)));
            rsx! { MixBar { playing: Some(ItemRef::Real(7)) } }
        });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(
            html.contains("pharos-mix-toggle") && html.contains(">Mix<"),
            "{html}"
        );
        assert!(
            html.contains(ICON_MIX),
            "the Mix button shows the die: {html}"
        );
    }

    #[test]
    fn a_mix_that_is_on_shows_its_name_and_a_stop_button() {
        let mut dom = VirtualDom::new(|| {
            use_context_provider(|| QueueCtx(Signal::new(Queue::default())));
            use_context_provider(|| {
                MixCtx(Signal::new(Some(ActiveMix {
                    exhausted: true,
                    ..on(MixSpec::default())
                })))
            });
            rsx! { MixBar { playing: Some(ItemRef::Real(1)) } }
        });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("Mix: Test"), "{html}");
        assert!(html.contains("Stop mix"), "{html}");
        assert!(html.contains("This mix has run out of songs."), "{html}");
    }

    /// B897/V846: the Playlists tab lists the presets first, then the
    /// listener's saved mixes in their saved order.
    #[test]
    fn the_shelf_lists_presets_then_saved_mixes() {
        let mine = vec![
            SavedMix::new("Late night", MixSpec::default()),
            SavedMix::new("Gym", MixSpec::default()),
        ];
        let tiles = shelf(&mine);
        let names: Vec<(&str, &str)> = tiles
            .iter()
            .map(|(k, m)| (k.label(), m.name.as_str()))
            .collect();
        let presets = presets().len();
        assert_eq!(tiles.len(), presets + 2);
        assert!(
            names[..presets].iter().all(|(k, _)| *k == "preset"),
            "{names:?}"
        );
        assert_eq!(
            names[presets..],
            [("saved", "Late night"), ("saved", "Gym")]
        );
        assert_ne!(ShelfKind::Preset.label(), ShelfKind::Saved.label());
    }

    #[test]
    fn a_mix_is_described_by_its_rules_with_their_values() {
        assert_eq!(describe(&MixSpec::default()), "Any song");
        let spec = MixSpec {
            rules: vec![
                Rule {
                    weight: Weight::Require,
                    share: 0,
                    test: Test::Genre("Trip-hop".into()),
                },
                Rule {
                    weight: Weight::Avoid,
                    share: 0,
                    test: Test::PlayCount {
                        min: 5,
                        max: u32::MAX,
                    },
                },
                Rule {
                    weight: Weight::Prefer,
                    share: 0,
                    test: Test::NotPlayedForDays(90),
                },
            ],
            ..MixSpec::default()
        };
        assert_eq!(
            describe(&spec),
            "Only Trip-hop · Less played 5+ times · More not played for 90 days"
        );
    }

    /// B897/V846: every preset is on the Playlists tab before any player
    /// opens, each a button that says what it picks.
    #[test]
    fn the_playlists_tab_shows_every_preset_as_a_play_button() {
        let mut dom = VirtualDom::new(|| {
            use_context_provider(|| QueueCtx(Signal::new(Queue::default())));
            use_context_provider(|| MixCtx(Signal::new(None)));
            rsx! { MixShelf {} }
        });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        for preset in presets() {
            assert!(
                html.contains(&format!(r#"aria-label="Play the mix {}""#, preset.name)),
                "{} missing: {html}",
                preset.name
            );
            assert!(html.contains(&describe(&preset.spec)), "{html}");
        }
        assert!(html.contains("Loading your mixes"), "{html}");
        assert!(
            !html.contains("pharos-mix-tile-on"),
            "nothing is playing: {html}"
        );
    }

    #[test]
    fn the_mix_that_is_on_says_so_on_its_tile() {
        let mut dom = VirtualDom::new(|| {
            use_context_provider(|| QueueCtx(Signal::new(Queue::default())));
            use_context_provider(|| {
                MixCtx(Signal::new(Some(ActiveMix {
                    name: "Favourites".to_owned(),
                    asking: true,
                    ..on(MixSpec::default())
                })))
            });
            rsx! { MixShelf {} }
        });
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert_eq!(html.matches("pharos-mix-tile-on").count(), 1, "{html}");
        assert!(html.contains("Finding songs…"), "{html}");
    }
}
