//! Playback on `MediaPlayer` and the `PharosPlayerListener` callbacks.

use super::*;

/// Starts `item` from `source` on a fresh `MediaPlayer`, unless a newer
/// play request than `gen` exists. Preparation is async;
/// `PharosPlayerListener.onPrepared` starts playback.
pub(crate) fn play(
    env: &mut JNIEnv,
    item: Item,
    source: &str,
    mut queue: Vec<Item>,
    gen: u64,
) -> R<()> {
    let mut guard = state_lock()?;
    if PLAY_GEN.load(Ordering::SeqCst) != gen {
        log(&format!(
            "play id={}: a newer request superseded it, dropped",
            item.id
        ));
        return Ok(());
    }
    let st = guard.as_mut().ok_or("play before onCreate")?;
    if let Some(old) = st.player.take() {
        resume::leave(env, "track_change")?;
        let _ = env.call_method(old.as_obj(), "release", "()V", &[]);
    }
    st.prepared = false;
    let classes = CLASSES.get().ok_or("play before onCreate")?;
    let player = env
        .new_object("android/media/MediaPlayer", "()V", &[])
        .map_err(jerr("MediaPlayer"))?;
    let listener_class: &JClass = classes.listener.as_obj().into();
    let listener = env
        .new_object(listener_class, "()V", &[])
        .map_err(jerr("PharosPlayerListener"))?;
    for (setter, iface) in [
        ("setOnPreparedListener", "OnPreparedListener"),
        ("setOnErrorListener", "OnErrorListener"),
        ("setOnCompletionListener", "OnCompletionListener"),
    ] {
        env.call_method(
            &player,
            setter,
            format!("(Landroid/media/MediaPlayer${iface};)V"),
            &[JValue::Object(&listener)],
        )
        .map_err(jerr(setter))?;
    }
    if let Err(e) = focus::apply(env, &player) {
        log(&format!("play id={}: no audio attributes: {e}", item.id));
    }
    let src = JObject::from(env.new_string(source).map_err(jerr("source"))?);
    env.call_method(
        &player,
        "setDataSource",
        "(Ljava/lang/String;)V",
        &[JValue::Object(&src)],
    )
    .map_err(jerr("setDataSource"))?;
    env.call_method(&player, "prepareAsync", "()V", &[])
        .map_err(jerr("prepareAsync"))?;
    st.player = Some(env.new_global_ref(&player).map_err(jerr("global player"))?);
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

/// Asks the library for `id`'s fresh stream URL (blocking) and plays it.
pub(crate) fn play_library(env: &mut JNIEnv, id: &str, queue: Vec<Item>, gen: u64) -> R<()> {
    let outcome = (|| {
        let lib = LIBRARY.get().ok_or("library not connected")?;
        if let Some(path) = lib.local_path(id) {
            let listed = queue
                .iter()
                .find(|i| i.id == id && !i.title.is_empty())
                .cloned();
            let item = match listed {
                Some(i) => i,
                None => Item::of_node(
                    lib.node(id)
                        .map_err(|e| format!("node {id} (cached, unlisted): {}", e.redacted()))?,
                ),
            };
            let ahead = lib.prefetch_from(id);
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
        let ahead = lib.prefetch_from(id);
        log(&format!(
            "play id={id} verdict=stream prefetch_queued={ahead}"
        ));
        play(env, Item::of_node(node), &url, queue, gen)
    })();
    if let Err(e) = &outcome {
        if PLAY_GEN.load(Ordering::SeqCst) == gen {
            snapshot::note_error(&format!("play {id}: {e}"));
            if let Some(session) = state_lock()?.as_ref().map(|s| s.session.clone()) {
                set_error(env, session.as_obj(), "Couldn't play that")?;
            }
        }
        log(&format!("play id={id} failed: {e}"));
    }
    Ok(())
}

/// Plays `id` with `queue` (or the remembered list holding it): a test
/// entry at once, a library node from a worker thread.
pub(crate) fn start(env: &mut JNIEnv, id: &str, queue: Option<Vec<Item>>) -> R<()> {
    let gen = PLAY_GEN.fetch_add(1, Ordering::SeqCst) + 1;
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

/// The state's session and current item when `mp` is still the current
/// player; `None` for a player `play` has already replaced.
pub(crate) fn if_current(
    env: &mut JNIEnv,
    mp: &JObject,
) -> R<Option<(GlobalRef, GlobalRef, Item)>> {
    let guard = state_lock()?;
    let Some(st) = guard.as_ref() else {
        return Ok(None);
    };
    let Some(player) = &st.player else {
        return Ok(None);
    };
    if !env
        .is_same_object(player.as_obj(), mp)
        .map_err(jerr("is_same_object"))?
    {
        return Ok(None);
    }
    let item = st.current.clone().ok_or("player without an item")?;
    Ok(Some((st.session.clone(), player.clone(), item)))
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
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    mp: JObject<'l>,
) {
    guarded(&mut env, "onPrepared", (), |env| {
        let Some((session, player, entry)) = if_current(env, &mp)? else {
            log("player onPrepared: stale player, not started");
            return Ok(());
        };
        let duration = env
            .call_method(player.as_obj(), "getDuration", "()I", &[])
            .and_then(|v| v.i())
            .map_err(jerr("getDuration"))?;
        if let Some(st) = state_lock()?.as_mut() {
            st.prepared = true;
        }
        let (length, source) = track_length(duration, entry.duration_ms);
        log(&format!(
            "duration source={source} ms={length} player_ms={duration} server_ms={} id={}",
            entry.duration_ms, entry.id
        ));
        set_metadata(env, session.as_obj(), &entry, length)?;
        snapshot::note_duration(length);
        let at = resume::prepared(env, &entry.id, u64::try_from(length).unwrap_or(0), &player)?;
        let at = at.and_then(|ms| i64::try_from(ms).ok()).unwrap_or(0);
        if !focus::request(env)? {
            set_playback_state(env, session.as_obj(), STATE_PAUSED, at)?;
            log(&format!(
                "player onPrepared id={}: audio focus refused, paused",
                entry.id
            ));
            return Ok(());
        }
        env.call_method(player.as_obj(), "start", "()V", &[])
            .map_err(jerr("start"))?;
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
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    mp: JObject<'l>,
    what: jni::sys::jint,
    extra: jni::sys::jint,
) -> jni::sys::jboolean {
    guarded(&mut env, "onError", (), |env| {
        let Some((session, _, entry)) = if_current(env, &mp)? else {
            log(&format!(
                "player onError what={what} extra={extra}: stale player, ignored"
            ));
            return Ok(());
        };
        if let Some(st) = state_lock()?.as_mut() {
            st.player = None;
            st.prepared = false;
        }
        resume::drop_current(false)?;
        let _ = env.call_method(&mp, "release", "()V", &[]);
        set_playback_state(env, session.as_obj(), STATE_ERROR, 0)?;
        snapshot::note_error(&format!("player error what={what} extra={extra}"));
        log(&format!(
            "player onError id={} what={what} extra={extra}: released, state ERROR",
            entry.id
        ));
        Ok(())
    });
    // Handled: without this MediaPlayer also calls onCompletion.
    1
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosPlayerListener_onCompletion<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    mp: JObject<'l>,
) {
    guarded(&mut env, "onCompletion", (), |env| {
        let Some((_, _, entry)) = if_current(env, &mp)? else {
            log("player onCompletion: stale player, ignored");
            return Ok(());
        };
        log(&format!(
            "player onCompletion id={}: next in the queue",
            entry.id
        ));
        resume::drop_current(true)?;
        skip(env, 1, false)
    })
}
