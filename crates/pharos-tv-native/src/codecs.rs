//! Decoder probe (spec 032 slice 3): list the device's decoders through
//! `android.media.MediaCodecList` and turn them into the `Capabilities` the
//! app negotiates with (`pharos_ui::native::decoders`).
//!
//! The NDK has no `MediaCodecList` before API 36, so this walks the framework
//! class over JNI (no Java written, as ADR-0061 and ADR-0077 require). Every
//! decoder and the resulting capabilities go to logcat under `pharos-bench`,
//! so a run on the TV records the list in `spike-0.md`.

use android_activity::AndroidApp;
use jni::JNIEnv;
use jni::objects::{JObject, JObjectArray, JString, JValue};
use pharos_client::api::Capabilities;
use pharos_ui::native::decoders::{self, DecoderInfo};

/// The capabilities the probe found, for the playback negotiation.
static CAPS: std::sync::OnceLock<Capabilities> = std::sync::OnceLock::new();

/// The probe's capabilities, waiting up to `wait` for it to finish; the CDD
/// fallback if it has not.
pub fn caps(wait: std::time::Duration) -> Capabilities {
    let until = std::time::Instant::now() + wait;
    loop {
        if let Some(c) = CAPS.get() {
            return c.clone();
        }
        if std::time::Instant::now() >= until {
            log::warn!("decoder probe not done; negotiating with the fallback");
            return decoders::capabilities(&[], false);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// `MediaCodecList.REGULAR_CODECS`: the codecs usable for ordinary playback,
/// not the tunneled or secure-only variants `ALL_CODECS` adds.
const REGULAR_CODECS: i32 = 0;

/// Probe the decoders and log them with the capabilities they give. A JNI
/// failure logs the error and returns the CDD fallback, never an empty list.
pub fn probe(app: &AndroidApp) -> Capabilities {
    let list = match list_decoders(app) {
        Ok(list) => list,
        Err(e) => {
            log::error!("decoder probe failed: {e}");
            Vec::new()
        }
    };
    for d in &list {
        log::info!(
            "decoder {} {} hw={} max={}x{}",
            d.name,
            d.mime,
            d.is_hardware(),
            d.max_width,
            d.max_height
        );
    }
    // HDR comes from the display (spec 032 open question 4), not probed yet.
    let caps = decoders::capabilities(&list, false);
    log::info!(
        "decoder caps video={:?} audio={:?} ({} decoders)",
        caps.video_codecs,
        caps.audio_codecs,
        list.len()
    );
    let _ = CAPS.set(caps.clone());
    caps
}

fn list_decoders(app: &AndroidApp) -> Result<Vec<DecoderInfo>, String> {
    // SAFETY: the pointer is the process's JavaVM, valid for its lifetime.
    let vm =
        unsafe { jni::JavaVM::from_raw(app.vm_as_ptr().cast()) }.map_err(|e| format!("vm: {e}"))?;
    let mut env = vm
        .attach_current_thread()
        .map_err(|e| format!("attach: {e}"))?;
    let result = walk(&mut env);
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    result.map_err(|e| format!("MediaCodecList: {e}"))
}

fn walk(env: &mut JNIEnv<'_>) -> jni::errors::Result<Vec<DecoderInfo>> {
    let list = env.new_object(
        "android/media/MediaCodecList",
        "(I)V",
        &[JValue::Int(REGULAR_CODECS)],
    )?;
    let infos: JObjectArray = env
        .call_method(
            &list,
            "getCodecInfos",
            "()[Landroid/media/MediaCodecInfo;",
            &[],
        )?
        .l()?
        .into();
    let n = env.get_array_length(&infos)?;
    let mut out = Vec::new();
    for i in 0..n {
        // One frame per codec keeps the local references bounded whatever
        // the list's length.
        env.with_local_frame(32, |env| -> jni::errors::Result<()> {
            let info = env.get_object_array_element(&infos, i)?;
            if env.call_method(&info, "isEncoder", "()Z", &[])?.z()? {
                return Ok(());
            }
            let name_obj = env
                .call_method(&info, "getName", "()Ljava/lang/String;", &[])?
                .l()?;
            let name = string(env, name_obj)?;
            let hardware = hardware_flag(env, &info);
            let types: JObjectArray = env
                .call_method(&info, "getSupportedTypes", "()[Ljava/lang/String;", &[])?
                .l()?
                .into();
            for t in 0..env.get_array_length(&types)? {
                let mime_obj = env.get_object_array_element(&types, t)?;
                let mime = string(env, mime_obj)?;
                let (max_width, max_height) = if mime.starts_with("video/") {
                    max_size(env, &info, &mime).unwrap_or_else(|e| {
                        log::warn!("decoder {name} {mime} size: {e}");
                        let _ = env.exception_clear();
                        (0, 0)
                    })
                } else {
                    (0, 0)
                };
                out.push(DecoderInfo {
                    name: name.clone(),
                    mime,
                    hardware,
                    max_width,
                    max_height,
                });
            }
            Ok(())
        })?;
    }
    Ok(out)
}

/// `isHardwareAccelerated()` is API 29; before it the method is missing,
/// and `DecoderInfo::is_hardware` falls back to the codec's name.
fn hardware_flag(env: &mut JNIEnv<'_>, info: &JObject<'_>) -> bool {
    match env
        .call_method(info, "isHardwareAccelerated", "()Z", &[])
        .and_then(|v| v.z())
    {
        Ok(hw) => hw,
        Err(_) => {
            let _ = env.exception_clear();
            false
        }
    }
}

/// The upper bounds of `VideoCapabilities.getSupportedWidths/Heights()`.
fn max_size(
    env: &mut JNIEnv<'_>,
    info: &JObject<'_>,
    mime: &str,
) -> jni::errors::Result<(u32, u32)> {
    let jmime = env.new_string(mime)?;
    let caps = env
        .call_method(
            info,
            "getCapabilitiesForType",
            "(Ljava/lang/String;)Landroid/media/MediaCodecInfo$CodecCapabilities;",
            &[JValue::Object(&jmime)],
        )?
        .l()?;
    let video = env
        .call_method(
            &caps,
            "getVideoCapabilities",
            "()Landroid/media/MediaCodecInfo$VideoCapabilities;",
            &[],
        )?
        .l()?;
    if video.is_null() {
        return Ok((0, 0));
    }
    let mut upper = |method: &str| -> jni::errors::Result<u32> {
        let range = env
            .call_method(&video, method, "()Landroid/util/Range;", &[])?
            .l()?;
        let value = env
            .call_method(&range, "getUpper", "()Ljava/lang/Comparable;", &[])?
            .l()?;
        let v = env.call_method(&value, "intValue", "()I", &[])?.i()?;
        Ok(u32::try_from(v).unwrap_or(0))
    };
    Ok((upper("getSupportedWidths")?, upper("getSupportedHeights")?))
}

fn string(env: &mut JNIEnv<'_>, obj: JObject<'_>) -> jni::errors::Result<String> {
    let s = JString::from(obj);
    let out: String = env.get_string(&s)?.into();
    Ok(out)
}
