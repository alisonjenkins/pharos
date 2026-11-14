//! `PharosSessionCallback`: transport, skip, seek and prepare, and the
//! session state and metadata the car reads.

use super::*;

macro_rules! callback {
    ($fn:ident, $name:literal, |$env:ident| $body:expr) => {
        #[no_mangle]
        pub extern "system" fn $fn<'l>(mut env: JNIEnv<'l>, _this: JObject<'l>) {
            log(concat!("session ", $name));
            guarded(&mut env, $name, (), |$env| $body)
        }
    };
}

pub(crate) fn set_playback_state(
    env: &mut JNIEnv,
    session: &JObject,
    state: i32,
    position_ms: i64,
) -> R<()> {
    publish_state(env, session, state, position_ms, None)
}

/// ERROR with `message`, which the car shows (and reads out) to the driver.
pub(crate) fn set_error(env: &mut JNIEnv, session: &JObject, message: &str) -> R<()> {
    publish_state(env, session, STATE_ERROR, 0, Some(message))
}

pub(crate) fn publish_state(
    env: &mut JNIEnv,
    session: &JObject,
    state: i32,
    position_ms: i64,
    error: Option<&str>,
) -> R<()> {
    snapshot::note_state(state, position_ms, error);
    const B: &str = "Landroid/media/session/PlaybackState$Builder;";
    let b = env
        .new_object("android/media/session/PlaybackState$Builder", "()V", &[])
        .map_err(jerr("PlaybackState.Builder"))?;
    let speed = if state == STATE_PLAYING { 1.0f32 } else { 0.0 };
    env.call_method(
        &b,
        "setState",
        format!("(IJF){B}"),
        &[
            JValue::Int(state),
            JValue::Long(position_ms),
            JValue::Float(speed),
        ],
    )
    .map_err(jerr("setState"))?;
    env.call_method(
        &b,
        "setActions",
        format!("(J){B}"),
        &[JValue::Long(ACTIONS)],
    )
    .map_err(jerr("setActions"))?;
    if let Some(message) = error {
        let m = JObject::from(env.new_string(message).map_err(jerr("error message"))?);
        env.call_method(
            &b,
            "setErrorMessage",
            format!("(Ljava/lang/CharSequence;){B}"),
            &[JValue::Object(&m)],
        )
        .map_err(jerr("setErrorMessage"))?;
    }
    let ps = env
        .call_method(&b, "build", "()Landroid/media/session/PlaybackState;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("PlaybackState.build"))?;
    env.call_method(
        session,
        "setPlaybackState",
        "(Landroid/media/session/PlaybackState;)V",
        &[JValue::Object(&ps)],
    )
    .map_err(jerr("setPlaybackState"))?;
    foreground::sync(env, state);
    Ok(())
}

pub(crate) fn set_metadata(
    env: &mut JNIEnv,
    session: &JObject,
    e: &Item,
    duration_ms: i64,
) -> R<()> {
    foreground::describe(&e.title, &e.subtitle)?;
    const B: &str = "Landroid/media/MediaMetadata$Builder;";
    let b = env
        .new_object("android/media/MediaMetadata$Builder", "()V", &[])
        .map_err(jerr("MediaMetadata.Builder"))?;
    let mut fields = vec![
        ("android.media.metadata.MEDIA_ID", e.id.clone()),
        ("android.media.metadata.TITLE", e.title.clone()),
        ("android.media.metadata.ARTIST", e.subtitle.clone()),
    ];
    if !e.album.is_empty() {
        fields.push(("android.media.metadata.ALBUM", e.album.clone()));
    }
    if !e.art.is_empty() {
        let uri = art::art_uri(&e.art);
        fields.push(("android.media.metadata.ALBUM_ART_URI", uri.clone()));
        fields.push(("android.media.metadata.DISPLAY_ICON_URI", uri));
    }
    for (key, value) in fields {
        let k = JObject::from(env.new_string(key).map_err(jerr("key"))?);
        let v = JObject::from(env.new_string(value).map_err(jerr("value"))?);
        env.call_method(
            &b,
            "putString",
            format!("(Ljava/lang/String;Ljava/lang/String;){B}"),
            &[JValue::Object(&k), JValue::Object(&v)],
        )
        .map_err(jerr("putString"))?;
    }
    let k = JObject::from(
        env.new_string("android.media.metadata.DURATION")
            .map_err(jerr("key"))?,
    );
    env.call_method(
        &b,
        "putLong",
        format!("(Ljava/lang/String;J){B}"),
        &[JValue::Object(&k), JValue::Long(duration_ms)],
    )
    .map_err(jerr("putLong"))?;
    let md = env
        .call_method(&b, "build", "()Landroid/media/MediaMetadata;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("MediaMetadata.build"))?;
    env.call_method(
        session,
        "setMetadata",
        "(Landroid/media/MediaMetadata;)V",
        &[JValue::Object(&md)],
    )
    .map_err(jerr("setMetadata"))?;
    Ok(())
}

/// The ids of `items`, in order.
pub(crate) fn ids(items: &[Item]) -> Vec<String> {
    items.iter().map(|i| i.id.clone()).collect()
}

/// Calls `method` on the current player, then publishes `state`.
pub(crate) fn transport(env: &mut JNIEnv, method: &str, state: i32) -> R<()> {
    let guard = state_lock()?;
    let st = guard.as_ref().ok_or("transport before onCreate")?;
    let session = st.session.clone();
    let Some(player) = st.player.clone() else {
        log(&format!("{method}: nothing loaded, ignored"));
        return Ok(());
    };
    if !st.prepared {
        if method != "stop" {
            log(&format!("{method}: player still preparing, ignored"));
            return Ok(());
        }
        drop(guard);
        if let Some(st) = state_lock()?.as_mut() {
            st.player = None;
        }
        env.call_method(player.as_obj(), "release", "()V", &[])
            .map_err(jerr("release"))?;
        log("stop: released a player still preparing");
        return set_playback_state(env, session.as_obj(), state, 0);
    }
    drop(guard);
    if method == "start" && !focus::request(env)? {
        log("start: audio focus refused, not started");
        return Ok(());
    }
    env.call_method(player.as_obj(), method, "()V", &[])
        .map_err(jerr(method))?;
    if method == "stop" {
        focus::abandon(env)?;
    }
    let pos = env
        .call_method(player.as_obj(), "getCurrentPosition", "()I", &[])
        .and_then(|v| v.i())
        .map_err(jerr("getCurrentPosition"))?;
    if method != "start" {
        resume::note(i64::from(pos), method)?;
    }
    if method == "stop" {
        resume::drop_current(false)?;
    }
    set_playback_state(env, session.as_obj(), state, i64::from(pos))
}

/// Moves `step` through the queue; past either end it wraps when `wrap`,
/// and stops otherwise. An empty queue plays the first test entry when the
/// test catalogue is built in, and does nothing otherwise.
pub(crate) fn skip(env: &mut JNIEnv, step: isize, wrap: bool) -> R<()> {
    let (current, queue) = {
        let guard = state_lock()?;
        let st = guard.as_ref().ok_or("skip before onCreate")?;
        (st.current.as_ref().map(|i| i.id.clone()), st.queue.clone())
    };
    if queue.is_empty() {
        let Some(first) = fixture::playables().into_iter().next() else {
            log("skip verdict=none reason=empty_queue");
            return Ok(());
        };
        return start(env, &first.id, None);
    }
    // A track played alone (from search, or offline) still has an album.
    if queue.len() == 1 && step == 1 {
        if let (Some(cur), Some(lib)) = (current.as_deref(), LIBRARY.get()) {
            if let Some(next) = lib.play_queue(cur).into_iter().next() {
                log("skip: next from the library's play queue");
                return start(env, &next, None);
            }
        }
    }
    let at = current
        .and_then(|c| queue.iter().position(|i| i.id == c))
        .unwrap_or(0);
    let len = queue.len() as isize;
    let next = at as isize + step;
    if !wrap && !(0..len).contains(&next) {
        log("skip: end of the queue, stopping");
        transport(env, "stop", STATE_STOPPED)?;
        snapshot::mark_ended();
        return Ok(());
    }
    let id = queue[next.rem_euclid(len) as usize].id.clone();
    start(env, &id, Some(queue))
}

/// Points the session at `PharosSearchActivity`, the app's only activity,
/// so the system has somewhere to send "open the app".
pub(crate) fn set_session_activity(env: &mut JNIEnv, ctx: &JObject, session: &JObject) -> R<()> {
    const FLAG_IMMUTABLE: i32 = 67_108_864;
    let class = env
        .find_class("com/alisonjenkins/pharos/auto/PharosSearchActivity")
        .map_err(jerr("find PharosSearchActivity"))?;
    let intent = env
        .new_object(
            "android/content/Intent",
            "(Landroid/content/Context;Ljava/lang/Class;)V",
            &[JValue::Object(ctx), JValue::Object(&class)],
        )
        .map_err(jerr("Intent"))?;
    let pi = env
        .call_static_method(
            "android/app/PendingIntent",
            "getActivity",
            "(Landroid/content/Context;ILandroid/content/Intent;I)Landroid/app/PendingIntent;",
            &[
                JValue::Object(ctx),
                JValue::Int(0),
                JValue::Object(&intent),
                JValue::Int(FLAG_IMMUTABLE),
            ],
        )
        .and_then(|v| v.l())
        .map_err(jerr("PendingIntent.getActivity"))?;
    env.call_method(
        session,
        "setSessionActivity",
        "(Landroid/app/PendingIntent;)V",
        &[JValue::Object(&pi)],
    )
    .map_err(jerr("setSessionActivity"))?;
    Ok(())
}

/// The keys of `extras`, for logs: which hints (focus, title, artist) the
/// assistant sent with a request.
pub(crate) fn bundle_keys(env: &mut JNIEnv, extras: &JObject) -> R<String> {
    if extras.is_null() {
        return Ok("null".into());
    }
    let keys = env
        .call_method(extras, "keySet", "()Ljava/util/Set;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("keySet"))?;
    let text = env
        .call_method(&keys, "toString", "()Ljava/lang/String;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("keySet.toString"))?;
    string(env, &JString::from(text))
}

/// Loads `item` without starting it, as the PREPARE actions ask: its
/// metadata and PAUSED. A later onPlay plays it.
pub(crate) fn prepare(env: &mut JNIEnv, item: Item, mut queue: Vec<Item>) -> R<()> {
    PLAY_GEN.fetch_add(1, Ordering::SeqCst);
    let session = {
        let mut guard = state_lock()?;
        let st = guard.as_mut().ok_or("prepare before onCreate")?;
        if let Some(old) = st.player.take() {
            let _ = env.call_method(old.as_obj(), "release", "()V", &[]);
        }
        st.prepared = false;
        if !queue.iter().any(|i| i.id == item.id) {
            queue = vec![item.clone()];
        }
        snapshot::note_track(&item.id, &ids(&queue), item.duration_ms);
        st.current = Some(item.clone());
        st.queue = queue;
        st.session.clone()
    };
    set_metadata(env, session.as_obj(), &item, item.duration_ms)?;
    set_playback_state(env, session.as_obj(), STATE_PAUSED, 0)?;
    log(&format!("prepared id={}", item.id));
    Ok(())
}

/// Prepares `id`: a test entry at once, a library node from a worker.
pub(crate) fn prepare_id(env: &mut JNIEnv, id: &str) -> R<()> {
    if let Some(item) = fixture::playable(id) {
        return prepare(env, item, fixture::playables());
    }
    let id = id.to_owned();
    on_worker("prepare", move |env| {
        let lib = LIBRARY.get().ok_or("library not connected")?;
        let node = lib
            .node(&id)
            .map_err(|e| format!("node {id}: {}", e.redacted()))?;
        prepare(env, Item::of_node(node), queue_for(&id))
    })
}

callback!(
    Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onPlay,
    "onPlay",
    |env| { apply(env, Command::Resume, Source::Car) }
);

callback!(
    Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onPause,
    "onPause",
    |env| { apply(env, Command::Pause, Source::Car) }
);

callback!(
    Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onStop,
    "onStop",
    |env| { apply(env, Command::Stop, Source::Car) }
);

callback!(
    Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onSkipToNext,
    "onSkipToNext",
    |env| { apply(env, Command::Next, Source::Car) }
);

callback!(
    Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onSkipToPrevious,
    "onSkipToPrevious",
    |env| { apply(env, Command::Previous, Source::Car) }
);

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onSeekTo<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    pos: jlong,
) {
    guarded(&mut env, "onSeekTo", (), |env| {
        log(&format!("session onSeekTo pos_ms={pos}"));
        apply(
            env,
            Command::Seek(u64::try_from(pos).unwrap_or(0)),
            Source::Car,
        )
    })
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onPlayFromMediaId<
    'l,
>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    id: JString<'l>,
    _extras: JObject<'l>,
) {
    guarded(&mut env, "onPlayFromMediaId", (), |env| {
        let id = string(env, &id)?;
        log(&format!("session onPlayFromMediaId id={id}"));
        apply(env, play_from(&id), Source::Car)
    })
}

callback!(
    Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onPrepare,
    "onPrepare",
    |env| {
        let current = state_lock()?.as_ref().and_then(|s| s.current.clone());
        match current {
            Some(item) => {
                let queue = state_lock()?
                    .as_ref()
                    .map(|s| s.queue.clone())
                    .unwrap_or_default();
                prepare(env, item, queue)
            }
            None => match fixture::playables().into_iter().next() {
                Some(first) => prepare(env, first, fixture::playables()),
                None => Ok(()),
            },
        }
    }
);

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onPrepareFromMediaId<
    'l,
>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    id: JString<'l>,
    _extras: JObject<'l>,
) {
    guarded(&mut env, "onPrepareFromMediaId", (), |env| {
        let id = string(env, &id)?;
        log(&format!("session onPrepareFromMediaId id={id}"));
        prepare_id(env, &id)
    })
}
