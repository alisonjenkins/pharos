//! Search and voice requests: ranked library hits, autoplay, and the
//! search activity Assistant opens.

use super::*;

/// Test matches, then the library's. Blocks on the network.
pub(crate) fn find(query: &str) -> Vec<Item> {
    find_ranked(query).0
}

/// [`find`], with the library's ranking when it searched.
pub(crate) fn find_ranked(query: &str) -> (Vec<Item>, Option<content::SearchReport>) {
    let mut hits = fixture::matches(query);
    let mut report = None;
    if let Some(lib) = LIBRARY.get() {
        match lib.search_ranked(query) {
            Ok((nodes, r)) => {
                hits.extend(nodes.into_iter().map(Item::of_node));
                report = r;
            }
            Err(e) => {
                log(&format!("library search failed: {e}"));
                if hits.is_empty() {
                    hits.push(Item::notice("Search failed", &e.to_string()));
                }
            }
        }
    }
    (hits, report)
}

/// Plays a search's top result unasked when it clearly beats the rest: a
/// track alone, an album or mix from its first track with the rest queued.
/// Blocks on the network to list an album; run it on a worker.
pub(crate) fn autoplay_search(
    env: &mut JNIEnv,
    query_len: usize,
    hits: &[Item],
    report: Option<&content::SearchReport>,
) -> R<()> {
    let (top, second) = report.map_or((0.0, 0.0), |r| (r.top_score, r.second_score));
    let tests = hits.iter().filter(|i| fixture::is_fixture(&i.id)).count();
    let (mut play_it, mut reason) = match report {
        None => (false, "no_ranking"),
        Some(_) if tests > 0 => (false, "test_matches"),
        Some(r) => content::autoplay(r, hits.len()),
    };
    let mut target: Option<(String, Vec<Item>)> = None;
    if play_it {
        match hits.first() {
            Some(t) if t.playable => target = Some((t.id.clone(), vec![t.clone()])),
            Some(t) if t.browsable => {
                let tracks: Vec<Item> =
                    children(&t.id).into_iter().filter(|i| i.playable).collect();
                match tracks.first() {
                    Some(first) => target = Some((first.id.clone(), tracks)),
                    None => (play_it, reason) = (false, "top_has_no_tracks"),
                }
            }
            _ => (play_it, reason) = (false, "top_not_playable"),
        }
    }
    let top_id = hits.first().map_or("none", |i| i.id.as_str());
    log(&format!(
        "search autoplay q_len={query_len} verdict={} reason={reason} top_score={top:.2} second_score={second:.2} returned={} top_id={top_id}",
        if play_it { "play" } else { "list" },
        hits.len()
    ));
    match target {
        Some((id, queue)) => start(env, &id, Some(queue)),
        None => Ok(()),
    }
}

/// Voice "play/prepare X": a test match at once, else the first playable
/// library hit, found on a worker. `then_play` plays it; otherwise it is
/// only prepared.
pub(crate) fn from_search(env: &mut JNIEnv, what: &str, query: String, then_play: bool) -> R<()> {
    if let Some(cmd) = voice::command(&query) {
        log(&format!("{what} verdict={cmd:?} reason=voice_command"));
        return voice::run(env, cmd);
    }
    let tests = fixture::matches(&query);
    if let Some(first) = tests.first() {
        log(&format!("{what} verdict={} reason=test_match", first.id));
        let id = first.id.clone();
        return if then_play {
            start(env, &id, Some(fixture::playables()))
        } else {
            prepare_id(env, &id)
        };
    }
    let gen = PLAY_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let what = what.to_owned();
    on_worker("voice search", move |env| {
        let hits = find(&query);
        remember(&hits);
        // A notice (search failed, sign in first) says more than "nothing matched".
        let notice = hits
            .iter()
            .find(|i| !i.playable && !i.browsable)
            .map(|i| i.title.clone());
        let queue: Vec<Item> = hits.into_iter().filter(|i| i.playable).collect();
        let Some(first) = queue.first().cloned() else {
            log(&format!("{what} verdict=None reason=no_playable_hit"));
            if PLAY_GEN.load(Ordering::SeqCst) == gen {
                if let Some(session) = state_lock()?.as_ref().map(|s| s.session.clone()) {
                    let message = notice.unwrap_or_else(|| format!("Nothing matched {query}"));
                    set_error(env, session.as_obj(), &message)?;
                }
            }
            return Ok(());
        };
        log(&format!(
            "{what} verdict={} reason=library_hit hits={}",
            first.id,
            queue.len()
        ));
        if then_play {
            play_library(env, &first.id, queue, gen)
        } else {
            prepare(env, first, queue)
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosMediaService_onSearch<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    query: JString<'l>,
    _extras: JObject<'l>,
    result: JObject<'l>,
) {
    guarded(&mut env, "onSearch", (), |env| {
        let q = string(env, &query)?;
        answer_later(env, "onSearch", &result, move || {
            let hits = find(&q);
            log(&format!("onSearch query={q:?} hits={}", hits.len()));
            remember(&hits);
            hits
        })
    })
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onPlayFromSearch<
    'l,
>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    query: JString<'l>,
    extras: JObject<'l>,
) {
    guarded(&mut env, "onPlayFromSearch", (), |env| {
        let q = string(env, &query)?;
        let keys = bundle_keys(env, &extras)?;
        log(&format!(
            "session onPlayFromSearch query={q:?} extras={keys}"
        ));
        from_search(env, "onPlayFromSearch", q, true)
    })
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosSessionCallback_onPrepareFromSearch<
    'l,
>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    query: JString<'l>,
    extras: JObject<'l>,
) {
    guarded(&mut env, "onPrepareFromSearch", (), |env| {
        let q = string(env, &query)?;
        let keys = bundle_keys(env, &extras)?;
        log(&format!(
            "session onPrepareFromSearch query={q:?} extras={keys}"
        ));
        from_search(env, "onPrepareFromSearch", q, false)
    })
}

/// The assistant's "play X on <app>" from the phone side arrives as a
/// MEDIA_PLAY_FROM_SEARCH intent to this invisible activity. It plays the
/// best match if the media service is running, and finishes either way.
#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosSearchActivity_nativeOnCreate<
    'l,
>(
    mut env: JNIEnv<'l>,
    this: JObject<'l>,
    _saved: JObject<'l>,
) {
    guarded(&mut env, "search activity", (), |env| {
        let intent = env
            .call_method(&this, "getIntent", "()Landroid/content/Intent;", &[])
            .and_then(|v| v.l())
            .map_err(jerr("getIntent"))?;
        let action = env
            .call_method(&intent, "getAction", "()Ljava/lang/String;", &[])
            .and_then(|v| v.l())
            .map_err(jerr("getAction"))?;
        let action = string(env, &JString::from(action))?;
        let key = JObject::from(env.new_string("query").map_err(jerr("key"))?);
        let q = env
            .call_method(
                &intent,
                "getStringExtra",
                "(Ljava/lang/String;)Ljava/lang/String;",
                &[JValue::Object(&key)],
            )
            .and_then(|v| v.l())
            .map_err(jerr("getStringExtra"))?;
        let q = string(env, &JString::from(q))?;
        let extras = env
            .call_method(&intent, "getExtras", "()Landroid/os/Bundle;", &[])
            .and_then(|v| v.l())
            .map_err(jerr("getExtras"))?;
        let keys = bundle_keys(env, &extras)?;
        let service_up = state_lock()?.is_some();
        let is_search = action == "android.media.action.MEDIA_PLAY_FROM_SEARCH";
        log(&format!(
            "search activity action={action} query={q:?} extras={keys} service_up={service_up}"
        ));
        // The session activity opens it without a search: show nothing, play nothing.
        let played = if service_up && is_search {
            from_search(env, "search activity", q, true)
        } else {
            Ok(())
        };
        env.call_method(&this, "finish", "()V", &[])
            .map_err(jerr("finish"))?;
        played
    })
}
