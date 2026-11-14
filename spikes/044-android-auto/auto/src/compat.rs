//! The slice of androidx's MediaBrowserServiceCompat protocol that Android
//! Auto needs for search. The platform `MediaBrowserService` has no search
//! call: a compat client searches only when the root extras carry a
//! Messenger binder (`extra_messenger`), and sends `MSG_SEARCH` to it with a
//! `ResultReceiver`. The reply goes back through that receiver's binder.
//!
//! Service version 1 keeps browsing on the framework path: a compat client
//! subscribes over the Messenger only from version 2.
//!
//! The request bundle names androidx's receiver class, which this app does
//! not have, so it is read from its parcel rather than unparcelled.

use std::sync::Mutex;

use jni::objects::{GlobalRef, JObject, JString, JValue};
use jni::JNIEnv;

use crate::{guarded, jerr, log, string, Item, R};

const EXTRA_MESSENGER: &str = "extra_messenger";
const EXTRA_SERVICE_VERSION: &str = "extra_service_version";
const SERVICE_VERSION: i32 = 1;

const MSG_SEARCH: i32 = 8;
const DATA_SEARCH_QUERY: &str = "data_search_query";
const DATA_RESULT_RECEIVER: &str = "data_result_receiver";
const SEARCH_RESULTS: &str = "search_results";

const RECEIVER_DESCRIPTOR: &str = "android.support.v4.os.IResultReceiver";
/// `IResultReceiver.send`, the interface's only (oneway) method.
const TRANSACTION_SEND: i32 = 1;
const FLAG_ONEWAY: i32 = 1;
const RESULT_OK: i32 = 0;

// Parcel value tags (android.os.Parcel.VAL_*).
const VAL_NULL: i32 = -1;
const VAL_STRING: i32 = 0;
const VAL_BUNDLE: i32 = 3;
const VAL_PARCELABLE: i32 = 4;

/// Tags written with an int length before the value (Parcel.isLengthPrefixed).
fn length_prefixed(tag: i32) -> bool {
    // MAP, PARCELABLE, LIST, SPARSEARRAY, PARCELABLEARRAY, OBJECTARRAY, SERIALIZABLE.
    matches!(tag, 2 | 4 | 11 | 12 | 16 | 17 | 21)
}

pub const ITEM_CLASS: &str = "android/support/v4/media/MediaBrowserCompat$MediaItem";

/// Keeps the Messenger (and so its Handler) alive for the process.
static MESSENGER: Mutex<Option<GlobalRef>> = Mutex::new(None);

/// Puts the Messenger binder and service version into the root `extras`.
/// Runs on the main thread, which the Handler then serves.
pub fn advertise(env: &mut JNIEnv, extras: &JObject) -> R<()> {
    let mut guard = MESSENGER.lock().map_err(|_| "messenger mutex poisoned".to_owned())?;
    if guard.is_none() {
        let handler = env
            .new_object("com/alisonjenkins/pharos/auto/PharosCompatHandler", "()V", &[])
            .map_err(jerr("PharosCompatHandler"))?;
        let messenger = env
            .new_object("android/os/Messenger", "(Landroid/os/Handler;)V", &[JValue::Object(&handler)])
            .map_err(jerr("Messenger"))?;
        *guard = Some(env.new_global_ref(&messenger).map_err(jerr("global messenger"))?);
    }
    let messenger = guard.as_ref().ok_or("no messenger")?.clone();
    drop(guard);
    let binder = env
        .call_method(messenger.as_obj(), "getBinder", "()Landroid/os/IBinder;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("getBinder"))?;
    let key = JObject::from(env.new_string(EXTRA_MESSENGER).map_err(jerr("key"))?);
    env.call_method(extras, "putBinder", "(Ljava/lang/String;Landroid/os/IBinder;)V", &[JValue::Object(&key), JValue::Object(&binder)])
        .map_err(jerr("putBinder"))?;
    let key = JObject::from(env.new_string(EXTRA_SERVICE_VERSION).map_err(jerr("key"))?);
    env.call_method(extras, "putInt", "(Ljava/lang/String;I)V", &[JValue::Object(&key), JValue::Int(SERVICE_VERSION)])
        .map_err(jerr("putInt"))?;
    Ok(())
}

fn parcel_int(env: &mut JNIEnv, p: &JObject) -> R<i32> {
    env.call_method(p, "readInt", "()I", &[]).and_then(|v| v.i()).map_err(jerr("readInt"))
}

fn parcel_string(env: &mut JNIEnv, p: &JObject) -> R<String> {
    let s = env
        .call_method(p, "readString", "()Ljava/lang/String;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("readString"))?;
    string(env, &JString::from(s))
}

fn parcel_position(env: &mut JNIEnv, p: &JObject) -> R<i32> {
    env.call_method(p, "dataPosition", "()I", &[]).and_then(|v| v.i()).map_err(jerr("dataPosition"))
}

fn parcel_seek(env: &mut JNIEnv, p: &JObject, pos: i32) -> R<()> {
    env.call_method(p, "setDataPosition", "(I)V", &[JValue::Int(pos)]).map_err(jerr("setDataPosition"))?;
    Ok(())
}

/// What a `MSG_SEARCH` bundle asks: the query, and the receiver's binder.
struct SearchRequest<'l> {
    query: String,
    receiver: JObject<'l>,
    /// The receiver's class as the client named it, for logs.
    receiver_class: String,
}

/// Walks `data`'s parcel for the query and the receiver's binder, skipping
/// every other value by its length.
fn read_search<'l>(env: &mut JNIEnv<'l>, data: &JObject, p: &JObject) -> R<SearchRequest<'l>> {
    env.call_method(data, "writeToParcel", "(Landroid/os/Parcel;I)V", &[JValue::Object(p), JValue::Int(0)])
        .map_err(jerr("Bundle.writeToParcel"))?;
    parcel_seek(env, p, 0)?;
    let len = parcel_int(env, p)?;
    if len <= 0 {
        return Err("search bundle is empty".into());
    }
    let _magic = parcel_int(env, p)?;
    let n = parcel_int(env, p)?;
    let mut query = None;
    let mut receiver = None;
    let mut receiver_class = String::new();
    for _ in 0..n {
        let key = parcel_string(env, p)?;
        let tag = parcel_int(env, p)?;
        match tag {
            VAL_NULL => {}
            VAL_STRING => {
                let v = parcel_string(env, p)?;
                if key == DATA_SEARCH_QUERY {
                    query = Some(v);
                }
            }
            // A bundle carries its own length, then its magic, then that many bytes.
            VAL_BUNDLE => {
                let l = parcel_int(env, p)?;
                if l > 0 {
                    let at = parcel_position(env, p)?;
                    parcel_seek(env, p, at + 4 + l)?;
                }
            }
            // INTEGER, SHORT, FLOAT, BOOLEAN: one 4-byte word (the pid, the uid).
            1 | 5 | 7 | 9 => {
                parcel_int(env, p)?;
            }
            // LONG, DOUBLE: 8 bytes.
            6 | 8 => {
                env.call_method(p, "readLong", "()J", &[]).map_err(jerr("readLong"))?;
            }
            t if length_prefixed(t) => {
                let l = parcel_int(env, p)?;
                let at = parcel_position(env, p)?;
                if key == DATA_RESULT_RECEIVER && t == VAL_PARCELABLE {
                    receiver_class = parcel_string(env, p)?;
                    let b = env
                        .call_method(p, "readStrongBinder", "()Landroid/os/IBinder;", &[])
                        .and_then(|v| v.l())
                        .map_err(jerr("readStrongBinder"))?;
                    receiver = Some(b);
                }
                parcel_seek(env, p, at + l)?;
            }
            t => return Err(format!("search bundle key {key:?} has tag {t}, which this reader cannot skip")),
        }
    }
    Ok(SearchRequest {
        query: query.unwrap_or_default(),
        receiver: receiver.filter(|r| !r.is_null()).ok_or("search bundle has no result receiver")?,
        receiver_class,
    })
}

fn obtain<'l>(env: &mut JNIEnv<'l>) -> R<JObject<'l>> {
    env.call_static_method("android/os/Parcel", "obtain", "()Landroid/os/Parcel;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("Parcel.obtain"))
}

/// Sends `items` to the client's receiver as `search_results`.
fn reply(env: &mut JNIEnv, receiver: &JObject, items: &[Item]) -> R<()> {
    let len = i32::try_from(items.len()).map_err(|_| "too many results")?;
    let arr = env
        .new_object_array(len, "android/os/Parcelable", JObject::null())
        .map_err(jerr("Parcelable[]"))?;
    for (i, e) in items.iter().enumerate() {
        let item = crate::media_item(env, e, true)?;
        let i = i32::try_from(i).map_err(|_| "too many results")?;
        env.set_object_array_element(&arr, i, item).map_err(jerr("set result"))?;
    }
    let bundle = env.new_object("android/os/Bundle", "()V", &[]).map_err(jerr("Bundle"))?;
    let key = JObject::from(env.new_string(SEARCH_RESULTS).map_err(jerr("key"))?);
    env.call_method(&bundle, "putParcelableArray", "(Ljava/lang/String;[Landroid/os/Parcelable;)V", &[JValue::Object(&key), JValue::Object(&arr)])
        .map_err(jerr("putParcelableArray"))?;
    let p = obtain(env)?;
    let token = JObject::from(env.new_string(RECEIVER_DESCRIPTOR).map_err(jerr("descriptor"))?);
    let sent = (|| {
        env.call_method(&p, "writeInterfaceToken", "(Ljava/lang/String;)V", &[JValue::Object(&token)])
            .map_err(jerr("writeInterfaceToken"))?;
        env.call_method(&p, "writeInt", "(I)V", &[JValue::Int(RESULT_OK)]).map_err(jerr("writeInt"))?;
        // A non-null `in Bundle` argument: 1, then the bundle.
        env.call_method(&p, "writeInt", "(I)V", &[JValue::Int(1)]).map_err(jerr("writeInt"))?;
        env.call_method(&bundle, "writeToParcel", "(Landroid/os/Parcel;I)V", &[JValue::Object(&p), JValue::Int(0)])
            .map_err(jerr("Bundle.writeToParcel"))?;
        env.call_method(
            receiver,
            "transact",
            "(ILandroid/os/Parcel;Landroid/os/Parcel;I)Z",
            &[JValue::Int(TRANSACTION_SEND), JValue::Object(&p), JValue::Object(&JObject::null()), JValue::Int(FLAG_ONEWAY)],
        )
        .and_then(|v| v.z())
        .map_err(jerr("transact"))
    })();
    let _ = env.call_method(&p, "recycle", "()V", &[]);
    let delivered = sent?;
    log(&format!("compat search reply results={} delivered={delivered}", items.len()));
    Ok(())
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosCompatHandler_handleMessage<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    msg: JObject<'l>,
) {
    guarded(&mut env, "compat handleMessage", (), |env| {
        let what = env.get_field(&msg, "what", "I").and_then(|v| v.i()).map_err(jerr("Message.what"))?;
        if what != MSG_SEARCH {
            log(&format!("compat message what={what}: ignored"));
            return Ok(());
        }
        let data = env
            .call_method(&msg, "getData", "()Landroid/os/Bundle;", &[])
            .and_then(|v| v.l())
            .map_err(jerr("getData"))?;
        let p = obtain(env)?;
        let req = read_search(env, &data, &p);
        let _ = env.call_method(&p, "recycle", "()V", &[]);
        let req = req?;
        let receiver = env.new_global_ref(&req.receiver).map_err(jerr("global receiver"))?;
        let (query, class) = (req.query, req.receiver_class);
        log(&format!("compat onSearch query={query:?} receiver={class}"));
        if let Some(cmd) = crate::voice::command(&query) {
            log(&format!("compat onSearch query={query:?} verdict={cmd:?} reason=voice_command"));
            crate::voice::run(env, cmd)?;
            return reply(env, receiver.as_obj(), &[Item::notice(cmd.done(), "Voice command")]);
        }
        // The library search blocks on the network; the main thread must not.
        crate::on_worker("compat search", move |env| {
            let hits = crate::find(&query);
            log(&format!("compat onSearch query={query:?} hits={}", hits.len()));
            crate::remember(&hits);
            reply(env, receiver.as_obj(), &hits)
        })
    })
}
