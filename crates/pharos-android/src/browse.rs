//! The browse tree: library children turned into car rows, and the
//! lists remembered so a played track can skip through its siblings.

use super::*;

pub(crate) fn description<'l>(env: &mut JNIEnv<'l>, e: &Item) -> R<JObject<'l>> {
    let b = env
        .new_object("android/media/MediaDescription$Builder", "()V", &[])
        .map_err(jerr("MediaDescription.Builder"))?;
    let id = JObject::from(env.new_string(&e.id).map_err(jerr("id"))?);
    let title = JObject::from(env.new_string(&e.title).map_err(jerr("title"))?);
    let sub = JObject::from(env.new_string(&e.subtitle).map_err(jerr("subtitle"))?);
    const B: &str = "Landroid/media/MediaDescription$Builder;";
    env.call_method(
        &b,
        "setMediaId",
        format!("(Ljava/lang/String;){B}"),
        &[JValue::Object(&id)],
    )
    .map_err(jerr("setMediaId"))?;
    env.call_method(
        &b,
        "setTitle",
        format!("(Ljava/lang/CharSequence;){B}"),
        &[JValue::Object(&title)],
    )
    .map_err(jerr("setTitle"))?;
    env.call_method(
        &b,
        "setSubtitle",
        format!("(Ljava/lang/CharSequence;){B}"),
        &[JValue::Object(&sub)],
    )
    .map_err(jerr("setSubtitle"))?;
    if !e.art.is_empty() {
        let icon = art::art_uri_object(env, &e.art)?;
        env.call_method(
            &b,
            "setIconUri",
            format!("(Landroid/net/Uri;){B}"),
            &[JValue::Object(&icon)],
        )
        .map_err(jerr("setIconUri"))?;
    }
    env.call_method(&b, "build", "()Landroid/media/MediaDescription;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("MediaDescription.build"))
}

/// A `MediaItem`: the framework's, or (`compat`) the androidx-named
/// subclass the compat search reply needs.
pub(crate) fn media_item<'l>(env: &mut JNIEnv<'l>, e: &Item, compat: bool) -> R<JObject<'l>> {
    let desc = description(env, e)?;
    let mut flags = 0;
    if e.browsable {
        flags |= FLAG_BROWSABLE;
    }
    if e.playable {
        flags |= FLAG_PLAYABLE;
    }
    const SIG: &str = "(Landroid/media/MediaDescription;I)V";
    let args = [JValue::Object(&desc), JValue::Int(flags)];
    if compat {
        let classes = CLASSES.get().ok_or("compat item before onCreate")?;
        let class: &JClass = classes.compat_item.as_obj().into();
        env.new_object(class, SIG, &args)
            .map_err(jerr("compat MediaItem"))
    } else {
        env.new_object("android/media/browse/MediaBrowser$MediaItem", SIG, &args)
            .map_err(jerr("MediaItem"))
    }
}

pub(crate) fn item_list<'l>(env: &mut JNIEnv<'l>, entries: &[Item]) -> R<JObject<'l>> {
    let list = env
        .new_object("java/util/ArrayList", "()V", &[])
        .map_err(jerr("ArrayList"))?;
    for e in entries {
        let item = media_item(env, e, false)?;
        env.call_method(
            &list,
            "add",
            "(Ljava/lang/Object;)Z",
            &[JValue::Object(&item)],
        )
        .map_err(jerr("ArrayList.add"))?;
    }
    Ok(list)
}

/// What `parent` holds. Blocks on the network for a library node.
pub(crate) fn children(parent: &str) -> Vec<Item> {
    let mut kids: Vec<Item> = if let Some(rows) = fixture::folder(parent) {
        rows
    } else {
        match LIBRARY.get() {
            None => vec![Item::notice(
                "Pharos is not connected",
                "See the app's logs",
            )],
            Some(lib) => match lib.children(parent) {
                Ok(nodes) => nodes.into_iter().map(Item::of_node).collect(),
                Err(e) => {
                    let why = e.redacted();
                    log(&format!("browse {parent} failed: {why}"));
                    vec![Item::notice("Couldn't load this list", &why)]
                }
            },
        }
    };
    if parent == ROOT {
        kids.extend(fixture::root_rows());
    }
    kids
}

/// Keeps `items`' playable rows so a later play can queue its siblings.
pub(crate) fn remember(items: &[Item]) {
    let playable: Vec<Item> = items.iter().filter(|i| i.playable).cloned().collect();
    if playable.is_empty() {
        return;
    }
    if let Ok(mut lists) = LISTS.lock() {
        lists.retain(|l| l.iter().map(|i| &i.id).ne(playable.iter().map(|i| &i.id)));
        lists.push(playable);
        let excess = lists.len().saturating_sub(REMEMBERED_LISTS);
        lists.drain(..excess);
    }
}

/// The newest remembered list holding `id`, else none.
pub(crate) fn queue_for(id: &str) -> Vec<Item> {
    LISTS
        .lock()
        .ok()
        .and_then(|lists| {
            lists
                .iter()
                .rev()
                .find(|l| l.iter().any(|i| i.id == id))
                .cloned()
        })
        .unwrap_or_default()
}

/// Runs `f` on a new thread attached to the JVM.
pub(crate) fn on_worker(
    name: &str,
    f: impl FnOnce(&mut JNIEnv) -> R<()> + Send + 'static,
) -> R<()> {
    let context = name.to_owned();
    std::thread::Builder::new()
        .name("pharos-auto-io".into())
        .spawn(move || {
            let Some(vm) = VM.get() else {
                log(&format!("{context} failed: no JavaVM"));
                return;
            };
            match vm.attach_current_thread() {
                Ok(mut env) => guarded(&mut env, &context, (), f),
                Err(e) => log(&format!("{context} failed: attach: {e}")),
            }
        })
        .map(|_| ())
        .map_err(|e| format!("spawn {name}: {e}"))
}

/// `result.sendResult(items)`; when building the list fails, an empty list,
/// so a detached result is never left unanswered.
pub(crate) fn send_items(env: &mut JNIEnv, result: &JObject, items: &[Item]) -> R<()> {
    let list = match item_list(env, items) {
        Ok(l) => l,
        Err(e) => {
            log(&format!("item list failed, sending an empty one: {e}"));
            if env.exception_check().unwrap_or(false) {
                let _ = env.exception_clear();
            }
            env.new_object("java/util/ArrayList", "()V", &[])
                .map_err(jerr("ArrayList"))?
        }
    };
    env.call_method(
        result,
        "sendResult",
        "(Ljava/lang/Object;)V",
        &[JValue::Object(&list)],
    )
    .map_err(jerr("sendResult"))?;
    Ok(())
}

/// Answers `result` with `work()` from a worker thread.
pub(crate) fn answer_later(
    env: &mut JNIEnv,
    name: &str,
    result: &JObject,
    work: impl FnOnce() -> Vec<Item> + Send + 'static,
) -> R<()> {
    env.call_method(result, "detach", "()V", &[])
        .map_err(jerr("detach"))?;
    let global = env.new_global_ref(result).map_err(jerr("global result"))?;
    let spawned = on_worker(name, move |env| send_items(env, global.as_obj(), &work()));
    if let Err(e) = spawned {
        log(&format!("{name}: {e}; answering now"));
        send_items(env, result, &[Item::notice("Couldn't load this list", &e)])?;
    }
    Ok(())
}

/// Tells the car the root and Music lists changed (sign-in finished).
pub(crate) fn notify_changed() {
    let Some(vm) = VM.get() else { return };
    let Ok(mut env) = vm.attach_current_thread() else {
        return;
    };
    guarded(&mut env, "notifyChildrenChanged", (), |env| {
        let Some(service) = state_lock()?.as_ref().map(|s| s.service.clone()) else {
            return Ok(());
        };
        for id in [ROOT, "music"] {
            let s = JObject::from(env.new_string(id).map_err(jerr("id"))?);
            env.call_method(
                service.as_obj(),
                "notifyChildrenChanged",
                "(Ljava/lang/String;)V",
                &[JValue::Object(&s)],
            )
            .map_err(jerr("notifyChildrenChanged"))?;
        }
        log("notifyChildrenChanged root, music");
        Ok(())
    });
}
