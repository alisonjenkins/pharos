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

/// Android 13 (API 33) length-prefixed these values for lazy bundles;
/// before it a parcelable's class name followed its tag directly.
const LENGTH_PREFIX_SDK: i32 = 33;

/// Tags written with an int length before the value (Parcel.isLengthPrefixed).
fn length_prefixed(tag: i32, sdk: i32) -> bool {
    // MAP, PARCELABLE, LIST, SPARSEARRAY, PARCELABLEARRAY, OBJECTARRAY, SERIALIZABLE.
    sdk >= LENGTH_PREFIX_SDK && matches!(tag, 2 | 4 | 11 | 12 | 16 | 17 | 21)
}

pub const ITEM_CLASS: &str = "android/support/v4/media/MediaBrowserCompat$MediaItem";

/// Keeps the Messenger (and so its Handler) alive for the process.
static MESSENGER: Mutex<Option<GlobalRef>> = Mutex::new(None);

/// Puts the Messenger binder and service version into the root `extras`.
/// Runs on the main thread, which the Handler then serves.
pub fn advertise(env: &mut JNIEnv, extras: &JObject) -> R<()> {
    let mut guard = MESSENGER
        .lock()
        .map_err(|_| "messenger mutex poisoned".to_owned())?;
    if guard.is_none() {
        let handler = env
            .new_object(
                "com/alisonjenkins/pharos/auto/PharosCompatHandler",
                "()V",
                &[],
            )
            .map_err(jerr("PharosCompatHandler"))?;
        let messenger = env
            .new_object(
                "android/os/Messenger",
                "(Landroid/os/Handler;)V",
                &[JValue::Object(&handler)],
            )
            .map_err(jerr("Messenger"))?;
        *guard = Some(
            env.new_global_ref(&messenger)
                .map_err(jerr("global messenger"))?,
        );
    }
    let messenger = guard.as_ref().ok_or("no messenger")?.clone();
    drop(guard);
    let binder = env
        .call_method(
            messenger.as_obj(),
            "getBinder",
            "()Landroid/os/IBinder;",
            &[],
        )
        .and_then(|v| v.l())
        .map_err(jerr("getBinder"))?;
    let key = JObject::from(env.new_string(EXTRA_MESSENGER).map_err(jerr("key"))?);
    env.call_method(
        extras,
        "putBinder",
        "(Ljava/lang/String;Landroid/os/IBinder;)V",
        &[JValue::Object(&key), JValue::Object(&binder)],
    )
    .map_err(jerr("putBinder"))?;
    let key = JObject::from(env.new_string(EXTRA_SERVICE_VERSION).map_err(jerr("key"))?);
    env.call_method(
        extras,
        "putInt",
        "(Ljava/lang/String;I)V",
        &[JValue::Object(&key), JValue::Int(SERVICE_VERSION)],
    )
    .map_err(jerr("putInt"))?;
    Ok(())
}

/// The reads `read_search` needs from a parcel.
trait ParcelSource {
    type Binder;
    fn int(&mut self) -> R<i32>;
    fn long(&mut self) -> R<()>;
    fn string(&mut self) -> R<String>;
    fn position(&mut self) -> R<i32>;
    fn seek(&mut self, pos: i32) -> R<()>;
    fn binder(&mut self) -> R<Self::Binder>;
}

struct JniParcel<'a, 'l> {
    env: &'a mut JNIEnv<'l>,
    p: &'a JObject<'l>,
}

impl<'l> ParcelSource for JniParcel<'_, 'l> {
    type Binder = JObject<'l>;

    fn int(&mut self) -> R<i32> {
        self.env
            .call_method(self.p, "readInt", "()I", &[])
            .and_then(|v| v.i())
            .map_err(jerr("readInt"))
    }

    fn long(&mut self) -> R<()> {
        self.env
            .call_method(self.p, "readLong", "()J", &[])
            .map_err(jerr("readLong"))?;
        Ok(())
    }

    fn string(&mut self) -> R<String> {
        let s = self
            .env
            .call_method(self.p, "readString", "()Ljava/lang/String;", &[])
            .and_then(|v| v.l())
            .map_err(jerr("readString"))?;
        string(self.env, &JString::from(s))
    }

    fn position(&mut self) -> R<i32> {
        self.env
            .call_method(self.p, "dataPosition", "()I", &[])
            .and_then(|v| v.i())
            .map_err(jerr("dataPosition"))
    }

    fn seek(&mut self, pos: i32) -> R<()> {
        self.env
            .call_method(self.p, "setDataPosition", "(I)V", &[JValue::Int(pos)])
            .map_err(jerr("setDataPosition"))?;
        Ok(())
    }

    fn binder(&mut self) -> R<JObject<'l>> {
        self.env
            .call_method(self.p, "readStrongBinder", "()Landroid/os/IBinder;", &[])
            .and_then(|v| v.l())
            .map_err(jerr("readStrongBinder"))
    }
}

/// What a `MSG_SEARCH` bundle asks: the query, and the receiver's binder.
struct SearchRequest<'l> {
    query: String,
    receiver: JObject<'l>,
    /// The receiver's class as the client named it, for logs.
    receiver_class: String,
}

struct Walked<B> {
    query: String,
    receiver: Option<B>,
    receiver_class: String,
}

fn android_sdk(env: &mut JNIEnv) -> R<i32> {
    env.get_static_field("android/os/Build$VERSION", "SDK_INT", "I")
        .and_then(|v| v.i())
        .map_err(jerr("SDK_INT"))
}

/// Walks `data`'s parcel for the query and the receiver's binder, skipping
/// every other value by its length.
fn read_search<'l>(env: &mut JNIEnv<'l>, data: &JObject, p: &JObject<'l>) -> R<SearchRequest<'l>> {
    let sdk = android_sdk(env)?;
    env.call_method(
        data,
        "writeToParcel",
        "(Landroid/os/Parcel;I)V",
        &[JValue::Object(p), JValue::Int(0)],
    )
    .map_err(jerr("Bundle.writeToParcel"))?;
    env.call_method(p, "setDataPosition", "(I)V", &[JValue::Int(0)])
        .map_err(jerr("setDataPosition"))?;
    let walked = walk_search(&mut JniParcel { env, p }, sdk)?;
    Ok(SearchRequest {
        query: walked.query,
        receiver: walked
            .receiver
            .filter(|r| !r.is_null())
            .ok_or("search bundle has no result receiver")?,
        receiver_class: walked.receiver_class,
    })
}

/// Reads the bundle's entries as `sdk` laid them out.
fn walk_search<S: ParcelSource>(p: &mut S, sdk: i32) -> R<Walked<S::Binder>> {
    let len = p.int()?;
    if len <= 0 {
        return Err("search bundle is empty".into());
    }
    let _magic = p.int()?;
    let n = p.int()?;
    let mut query = None;
    let mut receiver = None;
    let mut receiver_class = String::new();
    for _ in 0..n {
        let key = p.string()?;
        let tag = p.int()?;
        match tag {
            VAL_NULL => {}
            VAL_STRING => {
                let v = p.string()?;
                if key == DATA_SEARCH_QUERY {
                    query = Some(v);
                }
            }
            // A bundle carries its own length, then its magic, then that many bytes.
            VAL_BUNDLE => {
                let l = p.int()?;
                if l > 0 {
                    let at = p.position()?;
                    p.seek(at + 4 + l)?;
                }
            }
            // INTEGER, SHORT, FLOAT, BOOLEAN: one 4-byte word (the pid, the uid).
            1 | 5 | 7 | 9 => {
                p.int()?;
            }
            // LONG, DOUBLE: 8 bytes.
            6 | 8 => p.long()?,
            // Before API 33 a parcelable has no length: only the receiver, whose
            // class name and binder end it, can be read through.
            VAL_PARCELABLE if sdk < LENGTH_PREFIX_SDK => {
                if key != DATA_RESULT_RECEIVER {
                    return Err(format!(
                        "search bundle key {key:?} is a parcelable without a length, which this reader cannot skip"
                    ));
                }
                receiver_class = p.string()?;
                receiver = Some(p.binder()?);
            }
            t if length_prefixed(t, sdk) => {
                let l = p.int()?;
                let at = p.position()?;
                if key == DATA_RESULT_RECEIVER && t == VAL_PARCELABLE {
                    receiver_class = p.string()?;
                    receiver = Some(p.binder()?);
                }
                p.seek(at + l)?;
            }
            t => {
                return Err(format!(
                    "search bundle key {key:?} has tag {t}, which this reader cannot skip"
                ))
            }
        }
    }
    Ok(Walked {
        query: query.unwrap_or_default(),
        receiver,
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
        env.set_object_array_element(&arr, i, item)
            .map_err(jerr("set result"))?;
    }
    let bundle = env
        .new_object("android/os/Bundle", "()V", &[])
        .map_err(jerr("Bundle"))?;
    let key = JObject::from(env.new_string(SEARCH_RESULTS).map_err(jerr("key"))?);
    env.call_method(
        &bundle,
        "putParcelableArray",
        "(Ljava/lang/String;[Landroid/os/Parcelable;)V",
        &[JValue::Object(&key), JValue::Object(&arr)],
    )
    .map_err(jerr("putParcelableArray"))?;
    let p = obtain(env)?;
    let token = JObject::from(
        env.new_string(RECEIVER_DESCRIPTOR)
            .map_err(jerr("descriptor"))?,
    );
    let sent = (|| {
        env.call_method(
            &p,
            "writeInterfaceToken",
            "(Ljava/lang/String;)V",
            &[JValue::Object(&token)],
        )
        .map_err(jerr("writeInterfaceToken"))?;
        env.call_method(&p, "writeInt", "(I)V", &[JValue::Int(RESULT_OK)])
            .map_err(jerr("writeInt"))?;
        // A non-null `in Bundle` argument: 1, then the bundle.
        env.call_method(&p, "writeInt", "(I)V", &[JValue::Int(1)])
            .map_err(jerr("writeInt"))?;
        env.call_method(
            &bundle,
            "writeToParcel",
            "(Landroid/os/Parcel;I)V",
            &[JValue::Object(&p), JValue::Int(0)],
        )
        .map_err(jerr("Bundle.writeToParcel"))?;
        env.call_method(
            receiver,
            "transact",
            "(ILandroid/os/Parcel;Landroid/os/Parcel;I)Z",
            &[
                JValue::Int(TRANSACTION_SEND),
                JValue::Object(&p),
                JValue::Object(&JObject::null()),
                JValue::Int(FLAG_ONEWAY),
            ],
        )
        .and_then(|v| v.z())
        .map_err(jerr("transact"))
    })();
    let _ = env.call_method(&p, "recycle", "()V", &[]);
    let delivered = sent?;
    log(&format!(
        "compat search reply results={} delivered={delivered}",
        items.len()
    ));
    Ok(())
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosCompatHandler_handleMessage<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    msg: JObject<'l>,
) {
    guarded(&mut env, "compat handleMessage", (), |env| {
        let what = env
            .get_field(&msg, "what", "I")
            .and_then(|v| v.i())
            .map_err(jerr("Message.what"))?;
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
        let receiver = env
            .new_global_ref(&req.receiver)
            .map_err(jerr("global receiver"))?;
        let (query, class) = (req.query, req.receiver_class);
        log(&format!("compat onSearch query={query:?} receiver={class}"));
        if let Some(cmd) = crate::voice::command(&query) {
            log(&format!(
                "compat onSearch query={query:?} verdict={cmd:?} reason=voice_command"
            ));
            crate::voice::run(env, cmd)?;
            return reply(
                env,
                receiver.as_obj(),
                &[Item::notice(cmd.done(), "Voice command")],
            );
        }
        // The library search blocks on the network; the main thread must not.
        crate::on_worker("compat search", move |env| {
            let (hits, report) = crate::find_ranked(&query);
            log(&format!(
                "compat onSearch query={query:?} hits={}",
                hits.len()
            ));
            crate::remember(&hits);
            reply(env, receiver.as_obj(), &hits)?;
            crate::autoplay_search(env, query.chars().count(), &hits, report.as_ref())
        })
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    const RECEIVER_CLASS: &str = "android.support.v4.os.ResultReceiver";
    const BINDER: i32 = 0x0b1d;

    #[derive(Default)]
    struct Out(Vec<u8>);

    impl Out {
        fn int(&mut self, v: i32) -> &mut Self {
            self.0.extend_from_slice(&v.to_le_bytes());
            self
        }

        fn string(&mut self, s: &str) -> &mut Self {
            let units: Vec<u16> = s.encode_utf16().collect();
            self.int(i32::try_from(units.len()).unwrap());
            for u in &units {
                self.0.extend_from_slice(&u.to_le_bytes());
            }
            self.0.extend_from_slice(&[0, 0]);
            while self.0.len() % 4 != 0 {
                self.0.push(0);
            }
            self
        }
    }

    /// A bundle parcel: query, then the receiver, then a trailing int.
    /// `prefixed` writes the receiver as API 33 does, with a length.
    fn bundle(prefixed: bool) -> Vec<u8> {
        let mut o = Out::default();
        o.int(1).int(0x4c44_4e42).int(3);
        o.string(DATA_SEARCH_QUERY)
            .int(VAL_STRING)
            .string("a query");
        o.string(DATA_RESULT_RECEIVER).int(VAL_PARCELABLE);
        let mut receiver = Out::default();
        receiver.string(RECEIVER_CLASS).int(BINDER);
        if prefixed {
            o.int(i32::try_from(receiver.0.len()).unwrap());
        }
        o.0.extend_from_slice(&receiver.0);
        o.string("data_calling_pid").int(1).int(4242);
        let total = i32::try_from(o.0.len()).unwrap();
        o.0[0..4].copy_from_slice(&total.to_le_bytes());
        o.0
    }

    struct Bytes {
        data: Vec<u8>,
        at: usize,
    }

    impl ParcelSource for Bytes {
        type Binder = i32;

        fn int(&mut self) -> R<i32> {
            let b: [u8; 4] = self
                .data
                .get(self.at..self.at + 4)
                .and_then(|s| s.try_into().ok())
                .ok_or("read past the end")?;
            self.at += 4;
            Ok(i32::from_le_bytes(b))
        }

        fn long(&mut self) -> R<()> {
            self.at += 8;
            Ok(())
        }

        fn string(&mut self) -> R<String> {
            let n = usize::try_from(self.int()?).map_err(|_| "negative length")?;
            let end = self.at + n * 2;
            let units: Vec<u16> = self
                .data
                .get(self.at..end)
                .ok_or("read past the end")?
                .chunks(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            self.at = (end + 2).div_ceil(4) * 4;
            String::from_utf16(&units).map_err(|e| e.to_string())
        }

        fn position(&mut self) -> R<i32> {
            Ok(i32::try_from(self.at).unwrap())
        }

        fn seek(&mut self, pos: i32) -> R<()> {
            self.at = usize::try_from(pos).map_err(|_| "negative seek")?;
            Ok(())
        }

        fn binder(&mut self) -> R<i32> {
            self.int()
        }
    }

    fn walk(prefixed: bool, sdk: i32) -> R<Walked<i32>> {
        walk_search(
            &mut Bytes {
                data: bundle(prefixed),
                at: 0,
            },
            sdk,
        )
    }

    #[test]
    fn android_13_prefixes_the_receiver_with_its_length() {
        let w = walk(true, 33).unwrap();
        assert_eq!(w.query, "a query");
        assert_eq!(w.receiver, Some(BINDER));
        assert_eq!(w.receiver_class, RECEIVER_CLASS);
    }

    #[test]
    fn android_12_has_no_length_before_the_receivers_class_name() {
        for sdk in [26, 31, 32] {
            let w = walk(false, sdk).unwrap();
            assert_eq!(w.query, "a query", "sdk {sdk}");
            assert_eq!(w.receiver, Some(BINDER), "sdk {sdk}");
            assert_eq!(w.receiver_class, RECEIVER_CLASS, "sdk {sdk}");
        }
    }

    #[test]
    fn a_layout_read_as_the_wrong_release_is_an_error_not_a_guess() {
        assert!(walk(false, 33).is_err());
    }
}
