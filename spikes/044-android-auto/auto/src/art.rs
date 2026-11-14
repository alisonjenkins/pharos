//! Artwork for the car: `PharosArtProvider`, the app's ContentProvider.
//!
//! The car host draws item icons and album art from `content://` URIs it
//! opens through this provider, not from http URLs. [`art_uri`] turns an
//! image URL into `content://<AUTHORITY>/<key>`; `openFile` downloads the
//! image into the cache dir on first use and hands back a read-only file.
//!
//! Only URLs registered through [`art_uri`] in this process are fetched, so
//! the exported provider cannot be used to make the app request arbitrary
//! URLs. A file already in the cache is served by key after a restart.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use jni::objects::{JObject, JString, JValue};
use jni::sys::{jboolean, jint, jobject};
use jni::JNIEnv;

use crate::{jerr, log, string, R};

pub const AUTHORITY: &str = "com.alisonjenkins.pharos.auto.art";

/// How long a download may take to connect, and then between reads. Without
/// a limit `URLConnection` waits forever and pins the car host's binder thread.
const TIMEOUT_MS: i32 = 10_000;

/// `ParcelFileDescriptor.MODE_READ_ONLY`.
const MODE_READ_ONLY: i32 = 0x1000_0000;

/// Key -> the latest full URL registered for it.
static ALLOWED: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// The URL with any `api_key` query parameter removed: the token changes
/// every few hours, the image does not.
fn without_token(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else { return url.to_owned() };
    let kept: Vec<_> = query.split('&').filter(|p| !p.starts_with("api_key=")).collect();
    if kept.is_empty() {
        base.to_owned()
    } else {
        format!("{base}?{}", kept.join("&"))
    }
}

/// For logs: the URL without its token.
pub fn redact(url: &str) -> String {
    if url.contains("api_key=") {
        format!("{} [api_key redacted]", without_token(url))
    } else {
        url.to_owned()
    }
}

/// 16 hex digits of FNV-1a over the URL without its token.
fn key(url: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in without_token(url).bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

fn valid_key(k: &str) -> bool {
    k.len() == 16 && k.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The `content://` URI the car can open for image `http_url`. Cheap and
/// offline: it only records the URL, so it is safe on the main thread.
pub fn art_uri(http_url: &str) -> String {
    let k = key(http_url);
    if let Ok(mut map) = ALLOWED.lock() {
        map.get_or_insert_with(HashMap::new).insert(k.clone(), http_url.to_owned());
    }
    format!("content://{AUTHORITY}/{k}")
}

/// `Uri.parse(art_uri(http_url))`.
pub fn art_uri_object<'l>(env: &mut JNIEnv<'l>, http_url: &str) -> R<JObject<'l>> {
    let s = JObject::from(env.new_string(art_uri(http_url)).map_err(jerr("art uri"))?);
    env.call_static_method("android/net/Uri", "parse", "(Ljava/lang/String;)Landroid/net/Uri;", &[JValue::Object(&s)])
        .and_then(|v| v.l())
        .map_err(jerr("Uri.parse"))
}

fn java_string(env: &mut JNIEnv, obj: &JObject, method: &str) -> R<String> {
    let s = env
        .call_method(obj, method, "()Ljava/lang/String;", &[])
        .and_then(|v| v.l())
        .map_err(jerr(method))?;
    string(env, &JString::from(s))
}

fn file<'l>(env: &mut JNIEnv<'l>, path: &str) -> R<JObject<'l>> {
    let p = JObject::from(env.new_string(path).map_err(jerr("path"))?);
    env.new_object("java/io/File", "(Ljava/lang/String;)V", &[JValue::Object(&p)])
        .map_err(jerr("File"))
}

/// Downloads `url` to `dest` through a unique temp file, so two binder
/// threads fetching the same key never see a half-written image.
fn download(env: &mut JNIEnv, url: &str, dest: &str) -> R<()> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let tmp = format!("{dest}.{:?}.{nanos}.part", std::thread::current().id()).replace(['(', ')'], "");
    let u = JObject::from(env.new_string(url).map_err(jerr("url"))?);
    let u = env.new_object("java/net/URL", "(Ljava/lang/String;)V", &[JValue::Object(&u)]).map_err(jerr("URL"))?;
    let conn = env
        .call_method(&u, "openConnection", "()Ljava/net/URLConnection;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("openConnection"))?;
    env.call_method(&conn, "setConnectTimeout", "(I)V", &[JValue::Int(TIMEOUT_MS)])
        .map_err(jerr("setConnectTimeout"))?;
    env.call_method(&conn, "setReadTimeout", "(I)V", &[JValue::Int(TIMEOUT_MS)]).map_err(jerr("setReadTimeout"))?;
    let input = env
        .call_method(&conn, "getInputStream", "()Ljava/io/InputStream;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("getInputStream"))?;
    let tmp_file = file(env, &tmp)?;
    let tmp_path = env
        .call_method(&tmp_file, "toPath", "()Ljava/nio/file/Path;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("toPath"))?;
    let no_options = env
        .new_object_array(0, "java/nio/file/CopyOption", JObject::null())
        .map_err(jerr("CopyOption[]"))?;
    let copied = env
        .call_static_method(
            "java/nio/file/Files",
            "copy",
            "(Ljava/io/InputStream;Ljava/nio/file/Path;[Ljava/nio/file/CopyOption;)J",
            &[JValue::Object(&input), JValue::Object(&tmp_path), JValue::Object(&no_options)],
        )
        .and_then(|v| v.j())
        .map_err(jerr("Files.copy"));
    let _ = env.call_method(&input, "close", "()V", &[]);
    let bytes = match copied {
        Ok(n) => n,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    std::fs::rename(&tmp, dest).map_err(|e| format!("rename {tmp} -> {dest}: {e}"))?;
    log(&format!("art downloaded bytes={bytes} url={}", redact(url)));
    Ok(())
}

/// The cached image path for `uri`, downloading it first on a miss.
fn open(env: &mut JNIEnv, this: &JObject, uri: &JObject) -> R<String> {
    let k = java_string(env, uri, "getLastPathSegment")?;
    if !valid_key(&k) {
        return Err(format!("not an art key: {k:?}"));
    }
    let ctx = env
        .call_method(this, "getContext", "()Landroid/content/Context;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("getContext"))?;
    let dir = env
        .call_method(&ctx, "getCacheDir", "()Ljava/io/File;", &[])
        .and_then(|v| v.l())
        .map_err(jerr("getCacheDir"))?;
    let dir = format!("{}/art", java_string(env, &dir, "getAbsolutePath")?);
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {dir}: {e}"))?;
    let dest = format!("{dir}/{k}");
    if std::path::Path::new(&dest).exists() {
        log(&format!("art openFile key={k} verdict=cache-hit"));
        return Ok(dest);
    }
    let url = ALLOWED.lock().ok().and_then(|m| m.as_ref().and_then(|m| m.get(&k).cloned()));
    let Some(url) = url else {
        return Err(format!("art key {k} was never registered and is not cached"));
    };
    log(&format!("art openFile key={k} verdict=download"));
    download(env, &url, &dest)?;
    Ok(dest)
}

// ---- PharosArtProvider -----------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosArtProvider_onCreate<'l>(
    _env: JNIEnv<'l>,
    _this: JObject<'l>,
) -> jboolean {
    log("art provider onCreate");
    1
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosArtProvider_openFile<'l>(
    mut env: JNIEnv<'l>,
    this: JObject<'l>,
    uri: JObject<'l>,
    mode: JString<'l>,
) -> jobject {
    let result = (|| -> R<JObject<'l>> {
        let mode = string(&mut env, &mode)?;
        if mode != "r" {
            return Err(format!("art is read-only, asked for mode {mode:?}"));
        }
        let path = open(&mut env, &this, &uri)?;
        let f = file(&mut env, &path)?;
        env.call_static_method(
            "android/os/ParcelFileDescriptor",
            "open",
            "(Ljava/io/File;I)Landroid/os/ParcelFileDescriptor;",
            &[JValue::Object(&f), JValue::Int(MODE_READ_ONLY)],
        )
        .and_then(|v| v.l())
        .map_err(jerr("ParcelFileDescriptor.open"))
    })();
    match result {
        Ok(pfd) => pfd.into_raw(),
        Err(e) => {
            log(&format!("art openFile failed: {e}"));
            // A pending exception would make throw_new misbehave.
            if env.exception_check().unwrap_or(false) {
                let _ = env.exception_clear();
            }
            let _ = env.throw_new("java/io/FileNotFoundException", e);
            std::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosArtProvider_getType<'l>(
    env: JNIEnv<'l>,
    _this: JObject<'l>,
    _uri: JObject<'l>,
) -> jobject {
    // Callers ask for image/*; a null type fails openTypedAssetFile.
    env.new_string("image/*").map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosArtProvider_query<'l>(
    _env: JNIEnv<'l>,
    _this: JObject<'l>,
    _uri: JObject<'l>,
    _projection: JObject<'l>,
    _selection: JObject<'l>,
    _args: JObject<'l>,
    _order: JObject<'l>,
) -> jobject {
    std::ptr::null_mut()
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosArtProvider_insert<'l>(
    _env: JNIEnv<'l>,
    _this: JObject<'l>,
    _uri: JObject<'l>,
    _values: JObject<'l>,
) -> jobject {
    std::ptr::null_mut()
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosArtProvider_delete<'l>(
    _env: JNIEnv<'l>,
    _this: JObject<'l>,
    _uri: JObject<'l>,
    _selection: JObject<'l>,
    _args: JObject<'l>,
) -> jint {
    0
}

#[no_mangle]
pub extern "system" fn Java_com_alisonjenkins_pharos_auto_PharosArtProvider_update<'l>(
    _env: JNIEnv<'l>,
    _this: JObject<'l>,
    _uri: JObject<'l>,
    _values: JObject<'l>,
    _selection: JObject<'l>,
    _args: JObject<'l>,
) -> jint {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_ignores_the_token() {
        let a = "https://h/Items/1/Images/Primary?fillWidth=480&api_key=AAA";
        let b = "https://h/Items/1/Images/Primary?api_key=BBB&fillWidth=480";
        assert_eq!(without_token(a), "https://h/Items/1/Images/Primary?fillWidth=480");
        assert_eq!(key(a), key("https://h/Items/1/Images/Primary?fillWidth=480"));
        assert_eq!(key(a), key(b), "a token anywhere in the query is ignored");
        assert_ne!(key(a), key("https://h/Items/2/Images/Primary?fillWidth=480"));
        assert!(valid_key(&key(a)));
    }

    #[test]
    fn redact_hides_the_token() {
        assert!(!redact("https://h/a?api_key=SECRET").contains("SECRET"));
        assert_eq!(redact("https://h/a"), "https://h/a");
    }

    #[test]
    fn rejects_non_keys() {
        assert!(!valid_key("../../etc/passwd"));
        assert!(!valid_key("0123456789abcdeg"));
    }

    #[test]
    fn art_uri_registers() {
        let u = art_uri("https://h/x.jpg");
        let k = u.rsplit('/').next().unwrap();
        assert_eq!(u, format!("content://{AUTHORITY}/{k}"));
        assert!(ALLOWED.lock().unwrap().as_ref().unwrap().contains_key(k));
    }
}
