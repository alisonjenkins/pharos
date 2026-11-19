//! Playback on `MediaPlayer` and the `PharosPlayerListener` callbacks.

use std::sync::atomic::{AtomicBool, AtomicU64};

use super::*;

/// Whether the player being prepared should start once it is ready. A new
/// player wants to; a pause or headphone removal before it is ready says no.
static START_ON_PREPARED: AtomicBool = AtomicBool::new(true);

pub(crate) fn want_start() {
    START_ON_PREPARED.store(true, Ordering::SeqCst);
}

pub(crate) fn cancel_start() {
    START_ON_PREPARED.store(false, Ordering::SeqCst);
}

/// The newest play generation that asked to start at 0.
static FROM_START_GEN: AtomicU64 = AtomicU64::new(0);

pub(crate) fn want_from_start(gen: u64) {
    FROM_START_GEN.store(gen, Ordering::SeqCst);
}

fn asked_from_start(gen: u64) -> bool {
    gen != 0 && FROM_START_GEN.load(Ordering::SeqCst) == gen
}

pub(crate) fn start_wanted() -> bool {
    START_ON_PREPARED.load(Ordering::SeqCst)
}

/// Whether a player is preparing and will start when ready.
pub(crate) fn is_starting() -> R<bool> {
    let guard = state_lock()?;
    let preparing = guard
        .as_ref()
        .is_some_and(|s| s.player.is_some() && !s.prepared);
    Ok(will_start(preparing, start_wanted()))
}

fn will_start(preparing: bool, wanted: bool) -> bool {
    preparing && wanted
}

/// Takes the next play generation, which drops every older request still
/// in flight.
pub(crate) fn supersede() -> u64 {
    PLAY_GEN.fetch_add(1, Ordering::SeqCst) + 1
}

/// Whether `gen` is still the newest request.
pub(crate) fn is_current(gen: u64) -> bool {
    PLAY_GEN.load(Ordering::SeqCst) == gen
}

/// Starts `item` from `source` on a fresh `MediaPlayer`, unless a newer
/// play request than `gen` exists. Preparation is async;
/// `PharosPlayerListener.onPrepared` starts playback.
pub(crate) fn play(
    env: &mut Env,
    item: Item,
    source: &str,
    mut queue: Vec<Item>,
    gen: u64,
) -> R<()> {
    let mut guard = state_lock()?;
    if !is_current(gen) {
        log(&format!(
            "play id={}: a newer request superseded it, dropped",
            item.id
        ));
        return Ok(());
    }
    let st = guard.as_mut().ok_or("play before onCreate")?;
    if let Some(old) = st.player.take() {
        resume::leave(env, "track_change")?;
        let _ = env.call_method(
            old.as_obj(),
            jni::jni_str!("release"),
            jni::jni_sig!("()V"),
            &[],
        );
    }
    st.prepared = false;
    st.started = false;
    st.from_start = asked_from_start(gen);
    want_start();
    let classes = CLASSES.get().ok_or("play before onCreate")?;
    let player = env
        .new_object(
            jni::jni_str!("android/media/MediaPlayer"),
            jni::jni_sig!("()V"),
            &[],
        )
        .map_err(jerr("MediaPlayer"))?;
    let listener = env
        .new_object(&classes.listener, jni::jni_sig!("()V"), &[])
        .map_err(jerr("PharosPlayerListener"))?;
    env.call_method(
        &player,
        jni::jni_str!("setOnPreparedListener"),
        jni::jni_sig!("(Landroid/media/MediaPlayer$OnPreparedListener;)V"),
        &[JValue::Object(&listener)],
    )
    .map_err(jerr("setOnPreparedListener"))?;
    env.call_method(
        &player,
        jni::jni_str!("setOnErrorListener"),
        jni::jni_sig!("(Landroid/media/MediaPlayer$OnErrorListener;)V"),
        &[JValue::Object(&listener)],
    )
    .map_err(jerr("setOnErrorListener"))?;
    env.call_method(
        &player,
        jni::jni_str!("setOnCompletionListener"),
        jni::jni_sig!("(Landroid/media/MediaPlayer$OnCompletionListener;)V"),
        &[JValue::Object(&listener)],
    )
    .map_err(jerr("setOnCompletionListener"))?;
    if let Err(e) = apply_volume(env, &player) {
        log(&format!("play id={}: volume not set: {e}", item.id));
    }
    if let Err(e) = focus::apply(env, &player) {
        log(&format!("play id={}: no audio attributes: {e}", item.id));
    }
    let src = JObject::from(env.new_string(source).map_err(jerr("source"))?);
    env.call_method(
        &player,
        jni::jni_str!("setDataSource"),
        jni::jni_sig!("(Ljava/lang/String;)V"),
        &[JValue::Object(&src)],
    )
    .map_err(jerr("setDataSource"))?;
    env.call_method(
        &player,
        jni::jni_str!("prepareAsync"),
        jni::jni_sig!("()V"),
        &[],
    )
    .map_err(jerr("prepareAsync"))?;
    st.player = Some(GlobalRef::new(env, &player).map_err(jerr("global player"))?);
    if !queue.iter().any(|i| i.id == item.id) {
        queue = vec![item.clone()];
    }
    snapshot::note_track(&item.id, &ids(&queue), item.duration_ms);
    st.current = Some(item.clone());
    st.queue = queue;
    let session = st.session.clone();
    drop(guard);
    set_metadata(env, session.as_obj(), &item, item.duration_ms)?;
    set_playback_state(env, session.as_obj(), STATE_BUFFERING, 0)?;
    log(&format!(
        "preparing id={} source={}",
        item.id,
        art::redact(source)
    ));
    Ok(())
}

/// `id` as listed in `queue` when it is a book's part: the listing carries
/// the book, "Part N of M" and the server's position, which a fresh node
/// does not.
fn book_part(queue: &[Item], id: &str) -> Option<Item> {
    queue
        .iter()
        .find(|i| i.id == id && !i.book.is_empty())
        .cloned()
}

/// The row to show for a track whose file is cached: the listed book part
/// or titled row, else `lookup`'s node, else whatever the queue holds for
/// it, else a bare row. The file plays whatever the metadata, so an offline
/// library must not stop it.
fn cached_item(queue: &[Item], id: &str, lookup: impl FnOnce() -> Option<Item>) -> Item {
    book_part(queue, id)
        .or_else(|| {
            queue
                .iter()
                .find(|i| i.id == id && !i.title.is_empty())
                .cloned()
        })
        .or_else(lookup)
        .or_else(|| queue.iter().find(|i| i.id == id).cloned())
        .map(|mut i| {
            if i.title.is_empty() {
                i.title = FALLBACK_TITLE.to_owned();
            }
            i
        })
        .unwrap_or_else(|| Item {
            id: id.to_owned(),
            title: FALLBACK_TITLE.to_owned(),
            subtitle: String::new(),
            browsable: false,
            playable: true,
            art: String::new(),
            duration_ms: 0,
            album: String::new(),
            book: String::new(),
            position_ms: 0,
            group: String::new(),
        })
}

const FALLBACK_TITLE: &str = "Downloaded track";

/// `Library::prefetch_from`, unless the video accounting is not installed:
/// music would then spend room an admitted video needs. Refused with a log
/// line, as nothing queued.
fn prefetch_ahead(lib: &content::Library, id: &str) -> usize {
    // A video store that could not open at connect is tried again here, the
    // service-only process's only later chance.
    crate::service::retry_init("prefetch");
    if let Some(why) = prefetch_refusal(accounting_installed()) {
        log(&format!("prefetch refused id={id} reason={why}"));
        return 0;
    }
    lib.prefetch_from(id)
}

/// Asks the library for `id`'s fresh stream URL (blocking) and plays it.
pub(crate) fn play_library(env: &mut Env, id: &str, queue: Vec<Item>, gen: u64) -> R<()> {
    let queue_ids: Vec<String> = queue.iter().map(|i| i.id.clone()).collect();
    let outcome = (|| {
        let lib = LIBRARY.get().ok_or("library not connected")?;
        if let Some(path) = lib.local_path(id) {
            let item = cached_item(&queue, id, || match lib.node(id) {
                Ok(node) => Some(Item::of_node(node)),
                Err(e) => {
                    log(&format!(
                        "play id={id}: node lookup failed, using fallback metadata: {}",
                        e.redacted()
                    ));
                    None
                }
            });
            let ahead = prefetch_ahead(lib, id);
            log(&format!(
                "play id={id} verdict=cache-hit prefetch_queued={ahead}"
            ));
            return play(env, item, &path.to_string_lossy(), queue, gen);
        }
        if content::server_unreachable() {
            return Err(format!(
                "node {id}: not cached and the server is unreachable (debug offline)"
            ));
        }
        let node = lib
            .node(id)
            .map_err(|e| format!("node {id}: {}", e.redacted()))?;
        let url = node
            .stream_url
            .clone()
            .ok_or_else(|| format!("node {id} has no stream"))?;
        let ahead = prefetch_ahead(lib, id);
        log(&format!(
            "play id={id} verdict=stream prefetch_queued={ahead}"
        ));
        play(
            env,
            book_part(&queue, id).unwrap_or(Item::of_node(node)),
            &url,
            queue,
            gen,
        )
    })();
    if let Err(e) = &outcome {
        if is_current(gen) {
            snapshot::note_failure(id, &queue_ids, &format!("play {id}: {e}"));
            if let Some(session) = state_lock()?.as_ref().map(|s| s.session.clone()) {
                set_error(env, session.as_obj(), "Couldn't play that")?;
            }
        }
        log(&format!("play id={id} failed: {e}"));
    }
    Ok(())
}

/// Plays the book `id` (an `album/` id) from the part and place it stopped,
/// with every part queued, from a worker thread. A newer request made while
/// the book resolves wins; `gen` is the request's own generation, taken
/// when it was made, so an older book request cannot overtake a newer play.
pub(crate) fn play_book(id: &str, gen: u64, from_start: bool) -> R<()> {
    let id = id.to_owned();
    on_worker("book", move |env| {
        let lib = LIBRARY.get().ok_or("library not connected")?;
        let book = match lib.book(&id) {
            Ok(b) => b,
            Err(e) => {
                let why = format!("book {id}: {}", e.redacted());
                log(&format!("play {why}"));
                if is_current(gen) {
                    snapshot::note_error(&why);
                    if let Some(session) = state_lock()?.as_ref().map(|s| s.session.clone()) {
                        set_error(env, session.as_obj(), "Couldn't play that")?;
                    }
                }
                return Ok(());
            }
        };
        if !is_current(gen) {
            log(&format!(
                "play book {id}: a newer request superseded it, dropped"
            ));
            return Ok(());
        }
        let queue: Vec<Item> = book.parts.into_iter().map(Item::of_node).collect();
        remember(&queue);
        // Start over plays the book from its first part.
        let wanted_start = if from_start { 0 } else { book.start };
        let at = match start_index(queue.len(), wanted_start) {
            Ok(at) => at,
            Err(why) => {
                log(&format!("play book {id} verdict=rejected reason={why}"));
                snapshot::note_error(&why);
                return Ok(());
            }
        };
        let Some(first) = queue.get(at).map(|i| i.id.clone()) else {
            return Ok(());
        };
        log(&format!(
            "book id={id} parts={} start={}",
            queue.len(),
            book.start
        ));
        snapshot::clear_error();
        start_at(env, &first, Some(queue), gen)
    })
}

/// Whether `id` is a playable book (an `album/` row), which plays through
/// its parts rather than as one track.
pub(crate) fn is_book_id(id: &str) -> bool {
    id.starts_with("album/")
}

/// Plays `id` with `queue` (or the remembered list holding it): a test
/// entry at once, a library node from a worker thread.
pub(crate) fn start(env: &mut Env, id: &str, queue: Option<Vec<Item>>) -> R<()> {
    start_at(env, id, queue, supersede())
}

/// [`start`] that plays from 0 whatever position was saved for the item.
pub(crate) fn start_over(env: &mut Env, id: &str, queue: Option<Vec<Item>>) -> R<()> {
    let gen = supersede();
    want_from_start(gen);
    start_at(env, id, queue, gen)
}

/// [`start`] for a request that already holds its generation `gen`.
pub(crate) fn start_at(env: &mut Env, id: &str, queue: Option<Vec<Item>>, gen: u64) -> R<()> {
    if is_book_id(id) {
        log(&format!("start id={id}: a book, resolving its parts"));
        return play_book(id, gen, asked_from_start(gen));
    }
    if let Some(item) = fixture::playable(id) {
        let source = fixture::source(env, id)?;
        return play(
            env,
            item,
            &source,
            queue.unwrap_or_else(fixture::playables),
            gen,
        );
    }
    let id = id.to_owned();
    let queue = queue.unwrap_or_else(|| queue_for(&id));
    on_worker("play", move |env| play_library(env, &id, queue, gen))
}

/// Runs `effects` with the state, the player and the current item while
/// holding the state lock, when `mp` is still the current player; `None`
/// for a player `play` has already replaced. The identity check and the
/// effects share one critical section, so a stale callback cannot alter a
/// newer player's state.
fn while_current<T>(
    env: &mut Env,
    mp: &JObject,
    effects: impl FnOnce(&mut Env, &mut State, GlobalRef, Item) -> R<T>,
) -> R<Option<T>> {
    let mut guard = state_lock()?;
    let Some(st) = guard.as_mut() else {
        return Ok(None);
    };
    let Some(player) = st.player.clone() else {
        return Ok(None);
    };
    if !env
        .is_same_object(player.as_obj(), mp)
        .map_err(jerr("is_same_object"))?
    {
        return Ok(None);
    }
    let item = st.current.clone().ok_or("player without an item")?;
    effects(env, st, player, item).map(Some)
}

/// The length to show and where it came from. getDuration is -1 for a live
/// stream, so the server's length stands in.
pub(crate) fn track_length(player_ms: i32, server_ms: i64) -> (i64, &'static str) {
    if player_ms > 0 {
        (i64::from(player_ms), "player")
    } else {
        (server_ms, "server")
    }
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosPlayerListener_onPrepared<'l>(
    mut env: EnvUnowned<'l>,
    _this: JObject<'l>,
    mp: JObject<'l>,
) {
    guarded_native(&mut env, "onPrepared", (), |env| {
        let ready = while_current(env, &mp, |env, st, player, entry| {
            let duration = env
                .call_method(
                    player.as_obj(),
                    jni::jni_str!("getDuration"),
                    jni::jni_sig!("()I"),
                    &[],
                )
                .and_then(|v| v.i())
                .map_err(jerr("getDuration"))?;
            st.prepared = true;
            let (length, source) = track_length(duration, entry.duration_ms);
            log(&format!(
                "duration source={source} ms={length} player_ms={duration} server_ms={} id={}",
                entry.duration_ms, entry.id
            ));
            snapshot::note_duration(length);
            let at = resume::prepared(
                env,
                &entry.id,
                u64::try_from(length).unwrap_or(0),
                entry.position_ms,
                !entry.book.is_empty(),
                st.from_start,
                &player,
            )?;
            let at = at.and_then(|ms| i64::try_from(ms).ok()).unwrap_or(0);
            Ok((
                st.session.clone(),
                player,
                entry,
                length,
                at,
                start_wanted(),
            ))
        })?;
        let Some((session, player, entry, length, at, wanted)) = ready else {
            log("player onPrepared: stale player, not started");
            return Ok(());
        };
        set_metadata(env, session.as_obj(), &entry, length)?;
        if !wanted {
            set_playback_state(env, session.as_obj(), STATE_PAUSED, at)?;
            log(&format!(
                "player onPrepared id={}: paused while preparing, not started",
                entry.id
            ));
            return Ok(());
        }
        if !focus::request(env)? {
            set_playback_state(env, session.as_obj(), STATE_PAUSED, at)?;
            log(&format!(
                "player onPrepared id={}: audio focus refused, paused",
                entry.id
            ));
            return Ok(());
        }
        env.call_method(
            player.as_obj(),
            jni::jni_str!("start"),
            jni::jni_sig!("()V"),
            &[],
        )
        .map_err(jerr("start"))?;
        if let Some(st) = state_lock()?.as_mut() {
            st.started = true;
        }
        set_playback_state(env, session.as_obj(), STATE_PLAYING, at)?;
        log(&format!(
            "player onPrepared id={} duration_ms={length}: started",
            entry.id
        ));
        Ok(())
    })
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosPlayerListener_onError<'l>(
    mut env: EnvUnowned<'l>,
    _this: JObject<'l>,
    mp: JObject<'l>,
    what: jni::sys::jint,
    extra: jni::sys::jint,
) -> jni::sys::jboolean {
    guarded_native(&mut env, "onError", (), |env| {
        let gone = while_current(env, &mp, |_, st, _, entry| {
            st.player = None;
            st.prepared = false;
            st.started = false;
            resume::drop_current(false)?;
            Ok((st.session.clone(), entry))
        })?;
        let Some((session, entry)) = gone else {
            log(&format!(
                "player onError what={what} extra={extra}: stale player, ignored"
            ));
            return Ok(());
        };
        let _ = env.call_method(&mp, jni::jni_str!("release"), jni::jni_sig!("()V"), &[]);
        set_playback_state(env, session.as_obj(), STATE_ERROR, 0)?;
        snapshot::note_error(&format!("player error what={what} extra={extra}"));
        log(&format!(
            "player onError id={} what={what} extra={extra}: released, state ERROR",
            entry.id
        ));
        Ok(())
    });
    // Handled: without this MediaPlayer also calls onCompletion.
    true
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosPlayerListener_onCompletion<'l>(
    mut env: EnvUnowned<'l>,
    _this: JObject<'l>,
    mp: JObject<'l>,
) {
    guarded_native(&mut env, "onCompletion", (), |env| {
        let done = while_current(env, &mp, |_, _, _, entry| {
            resume::drop_current(true)?;
            Ok(entry)
        })?;
        let Some(entry) = done else {
            log("player onCompletion: stale player, ignored");
            return Ok(());
        };
        log(&format!(
            "player onCompletion id={}: next in the queue",
            entry.id
        ));
        skip(env, 1, false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_newer_request_supersedes_every_older_generation() {
        let first = supersede();
        assert!(is_current(first));
        let second = supersede();
        assert!(!is_current(first), "the older request is dropped");
        assert!(is_current(second));
    }

    fn row(id: &str, title: &str) -> Item {
        Item {
            id: id.to_owned(),
            title: title.to_owned(),
            subtitle: String::new(),
            browsable: false,
            playable: true,
            art: String::new(),
            duration_ms: 0,
            album: String::new(),
            book: String::new(),
            position_ms: 0,
            group: String::new(),
        }
    }

    #[test]
    fn a_cached_track_plays_with_fallback_metadata_when_the_library_is_offline() {
        let untitled = [row("track/1", "")];
        let bare = cached_item(&untitled, "track/1", || None);
        assert_eq!(bare.id, "track/1");
        assert!(!bare.title.is_empty(), "something to show");
        let unlisted = cached_item(&[], "track/2", || None);
        assert_eq!(unlisted.id, "track/2");
        assert!(!unlisted.title.is_empty());
    }

    #[test]
    fn a_titled_row_or_a_found_node_wins_over_the_fallback() {
        let listed = [row("track/1", "Listed")];
        let looked_up = std::cell::Cell::new(false);
        let item = cached_item(&listed, "track/1", || {
            looked_up.set(true);
            None
        });
        assert_eq!(item.title, "Listed");
        assert!(!looked_up.get(), "no lookup for a listed row");
        let found = cached_item(&[], "track/2", || Some(row("track/2", "Found")));
        assert_eq!(found.title, "Found");
    }

    #[test]
    fn an_album_id_is_a_book_and_a_track_is_not() {
        assert!(is_book_id("album/b1"));
        assert!(!is_book_id("track/1"));
        assert!(!is_book_id("tone:440"));
    }

    #[test]
    fn only_a_preparing_player_that_is_wanted_will_start() {
        assert!(will_start(true, true));
        assert!(!will_start(true, false), "paused while preparing");
        assert!(!will_start(false, true), "already prepared or none");
    }

    #[test]
    fn a_pause_while_preparing_cancels_the_start_until_the_next_player() {
        want_start();
        assert!(start_wanted(), "a new player starts when ready");
        cancel_start();
        assert!(!start_wanted());
        want_start();
        assert!(start_wanted());
    }
}
