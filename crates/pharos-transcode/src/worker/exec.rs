//! Worker-side job execution helpers shared by the `transcode-worker`
//! binary. The spawn build shells out to the `ffmpeg` binary (crash-
//! isolated by the process boundary); the `backend-lib` build will use
//! the in-process FFI path (added in a later step) but reuses the same
//! device→hwaccel mapping + failure classification.

use crate::ffmpeg_transcode_args;
use crate::hwaccel::detect_available;
use crate::protocol::{DeviceId, JobSpec, OutputSink, WorkerError};
use smallvec::SmallVec;
use std::path::PathBuf;

/// ffmpeg binary the worker invokes. Overridable so tests + the nix
/// devShell can pin a specific build.
pub fn ffmpeg_bin() -> String {
    std::env::var("PHAROS_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_string())
}

/// Devices this worker can actually use, advertised at handshake. CPU is
/// always last/terminal; hardware families come from `ffmpeg -hwaccels`
/// expanded per GPU (one VAAPI device per DRM render node).
pub async fn openable_devices() -> SmallVec<[DeviceId; 4]> {
    let detected = detect_available(&ffmpeg_bin()).await;
    let mut v: SmallVec<[DeviceId; 4]> = crate::device::enumerate(&detected).into_iter().collect();
    v.push(DeviceId::Cpu);
    v
}

/// True for lines ffmpeg's dependencies emit in bulk and that never name the
/// reason a run failed.
///
/// ffmpeg does not stop writing once it has reported its fatal error, so a
/// library that repeats itself can push that error clean out of any
/// fixed-size tail. A burn-in whose fontconfig had no writable cache did
/// exactly that: the reported error was four repetitions of "No writable
/// cache directories" and the actual cause was never visible.
fn is_stderr_noise(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty()
        // Continuation lines of a multi-line library complaint.
        || line.starts_with('\t')
        || trimmed.starts_with("Fontconfig error")
        || trimmed.starts_with("Fontconfig warning")
        // ffmpeg's own periodic progress and per-frame chatter.
        || trimmed.starts_with("frame=")
        || trimmed.starts_with("size=")
        || trimmed.starts_with("Past duration")
        || trimmed.starts_with("Last message repeated")
}

/// `stderr` with the known-noise lines removed. Falls back to the original
/// when filtering would leave nothing — reporting noise beats reporting an
/// empty string.
fn without_noise(stderr: &str) -> String {
    let kept = stderr
        .lines()
        .filter(|l| !is_stderr_noise(l))
        .collect::<Vec<_>>()
        .join("\n");
    if kept.trim().is_empty() {
        stderr.to_owned()
    } else {
        kept
    }
}

fn tail_chars(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.trim_end().chars().collect();
    let start = chars.len().saturating_sub(max_chars);
    chars[start..].iter().collect()
}

/// Map an ffmpeg failure (stderr text + nonzero exit) to a `WorkerError`
/// the scheduler can route on.
///
/// Classification rules, in order:
/// 1. A **hard source-decode error** (corrupt/missing input, missing
///    decoder) is `BadInput` — non-recoverable; it would fail on every
///    device, so retrying elsewhere is pointless.
/// 2. A **decode** that failed on its own terms — the decoder ran and could
///    not produce pictures — is `DecodeFailed`, on any device. This is
///    checked BEFORE the hardware arm below, and that order is the whole
///    point: it is the one non-source failure that is not evidence about the
///    device.
/// 3. Otherwise, a failure on a **hardware device** is `DeviceBusy`
///    (transient) so the scheduler falls back to the next device / CPU.
///    Real hardware faults are myriad and version-specific ("Cannot load
///    libcuda.so.1", "no capable devices", VAAPI format-link errors, out
///    of sessions, …); rather than enumerate them, we treat any
///    non-source-error HW failure as a reason to fall back. The CPU is
///    the terminal device, so a genuinely broken job still surfaces a
///    real error there.
/// 4. A failure on the **CPU** device that isn't a source error is
///    `Other` — a genuine, non-recoverable encode error.
pub fn classify_failure(stderr: &str, is_hw: bool) -> WorkerError {
    // Drop the chatter first: the reported window is only useful if it is
    // spent on lines that can name a cause.
    let meaningful = without_noise(stderr);
    // The stderr tail is the actual ffmpeg reason — carry it on every
    // classification so the log names the cause, never a bare class.
    let tail = tail_chars(&meaningful, 400);
    // Classify on the WHOLE dump, not the reported tail: a decisive line can
    // sit further back than the window we quote.
    let s = stderr.to_ascii_lowercase();
    let hard_bad_input = s.contains("invalid data found")
        || s.contains("could not find codec")
        || s.contains("decoder not found")
        || s.contains("no such file")
        || s.contains("unable to find a suitable output format");
    if hard_bad_input {
        return WorkerError::BadInput(tail);
    }
    // A decoder that ran and could not produce pictures. Both strings name the
    // DECODE side explicitly, which is what makes them safe to lift out of the
    // hardware arm below:
    //
    // - "Decode error rate N exceeds maximum M" is ffmpeg's verdict after
    //   counting failed frames against the `max_error_rate` threshold — the
    //   decisive line in the 2026-08-01 incident, where an AV1 source was sent
    //   to a Pascal card that has no AV1 decode block and every single frame
    //   failed.
    // - "hwaccel initialisation returned error" is NVDEC/VAAPI reporting that
    //   this DECODER could not be set up on this device — a codec answer, not
    //   a health answer. The card encodes fine either side of it.
    //
    // Classified before the `is_hw` arm because that arm cools the device, and
    // a source the device cannot decode is not a reason to stop encoding
    // everything else on it.
    if s.contains("decode error rate") || s.contains("hwaccel initialisation returned error") {
        return WorkerError::DecodeFailed(tail);
    }
    if is_hw {
        // Carry the tail here too. This is the branch the doc comment above is
        // about and the branch that used to drop it: `DeviceBusy` takes a
        // device out of service, so it is the LAST error that should have been
        // silent about why.
        return WorkerError::DeviceBusy(tail);
    }
    // CPU failure that isn't a source error — non-recoverable encode error.
    WorkerError::Other(tail)
}

/// Resolved output target for a spawn job.
pub enum SpawnTarget {
    /// Write to this file path (FileDirect / cached segment).
    File(PathBuf),
    /// Write the muxed stream to the worker's stdout (live path).
    Stdout,
}

/// Build the ffmpeg argv + resolve the output target for a spawn job.
/// Errors map to the appropriate non-transient class.
pub fn spawn_job_args(spec: &JobSpec) -> Result<(Vec<String>, SpawnTarget), WorkerError> {
    let input = spec
        .input
        .to_str()
        .ok_or_else(|| WorkerError::BadInput(format!("non-utf8 input path: {:?}", spec.input)))?;
    match &spec.sink {
        OutputSink::FileDirect { path } => {
            let out_str = path
                .to_str()
                .ok_or_else(|| WorkerError::BadInput(format!("non-utf8 output path: {path:?}")))?;
            let args =
                ffmpeg_transcode_args(input, &spec.opts, spec.device, out_str, spec.decode_offload);
            Ok((args, SpawnTarget::File(path.clone())))
        }
        OutputSink::Stdout => {
            let args = ffmpeg_transcode_args(
                input,
                &spec.opts,
                spec.device,
                "pipe:1",
                spec.decode_offload,
            );
            Ok((args, SpawnTarget::Stdout))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hardware failure is transient — the scheduler retries elsewhere — AND
    /// it says what went wrong.
    ///
    /// The second half is the one that matters. `DeviceBusy` is the only error
    /// that takes a device out of service, and it used to be a unit variant:
    /// `classify_failure` computed the stderr tail, gave it to `BadInput` and
    /// `Other`, and dropped it here. So the most consequential failure in the
    /// scheduler was the only one that could not name its cause, and a real
    /// incident had to be diagnosed by reading this function rather than a log.
    ///
    /// These three are genuinely different problems — a missing driver, a
    /// filter-graph mismatch, an exhausted card — and an operator who cannot
    /// tell them apart cannot act on any of them.
    #[test]
    fn a_hardware_failure_is_transient_and_carries_its_cause() {
        for (stderr, expect) in [
            ("[h264_nvenc] Cannot load libcuda.so.1", "libcuda"),
            (
                "Impossible to convert between the formats",
                "convert between the formats",
            ),
            ("OpenEncodeSessionEx failed: out of memory", "out of memory"),
        ] {
            let e = classify_failure(stderr, true);
            assert!(
                e.is_transient(),
                "{stderr} must fall back to another device"
            );
            let WorkerError::DeviceBusy(why) = &e else {
                panic!("{stderr} classified as {e:?}, not DeviceBusy");
            };
            assert!(
                why.contains(expect),
                "the cause must survive classification: wanted {expect:?} in {why:?}"
            );
            // And it reaches a log line, not just the struct.
            assert!(
                e.to_string().contains(expect),
                "Display drops the cause: {e}"
            );
        }
    }

    #[test]
    fn hard_source_error_is_bad_input_on_any_device() {
        assert!(matches!(
            classify_failure("Invalid data found when processing input", true),
            WorkerError::BadInput(s) if s.contains("Invalid data found")
        ));
        assert!(matches!(
            classify_failure("x.mkv: No such file or directory", false),
            WorkerError::BadInput(s) if s.contains("No such file")
        ));
    }

    /// The 2026-08-01 outage, verbatim. An AV1 source was placed on a GTX 1070
    /// (Pascal — no AV1 decode block), every frame failed to decode, and the
    /// failure was classified as a HARDWARE fault. That cooled the whole GPU
    /// for two seconds, and under 007's shared-init pin a cooled device does
    /// not slow a rendition down, it FAILS it: 453 client segments of unrelated
    /// titles died because one file could not be decoded.
    ///
    /// So the assertion that matters is `!is_transient()` — that is the bit the
    /// scheduler reads to decide whether to cool the device. Without the
    /// `DecodeFailed` arm this stderr classifies as `DeviceBusy`, which IS
    /// transient, and this test goes red.
    #[test]
    fn a_decode_failure_does_not_take_the_device_out_of_service() {
        let stderr = "[dec:av1 @ 0x7f2c] Decode error rate 1 exceeds maximum 0.666667\n\
                      [dec:av1 @ 0x7f2c] Task finished with error code: -1145393733\n";
        for is_hw in [true, false] {
            let e = classify_failure(stderr, is_hw);
            assert!(
                !e.is_transient(),
                "a source the decoder cannot handle must not cool the device \
                 (is_hw={is_hw}), got {e:?}"
            );
            let WorkerError::DecodeFailed(why) = &e else {
                panic!("expected DecodeFailed on is_hw={is_hw}, got {e:?}");
            };
            // The offending value, not a bare class: "decode failed" alone does
            // not say which decoder or how badly, and that is the first thing
            // asked.
            assert!(
                why.contains("Decode error rate"),
                "the cause must survive classification, got {why:?}"
            );
            assert!(
                e.to_string().contains("Decode error rate"),
                "Display drops the cause: {e}"
            );
        }
        assert_eq!(
            classify_failure(stderr, true).label(),
            "decode_failed",
            "the dashboard must be able to tell a dead card from an undecodable \
             source; they were one label during the outage"
        );
        assert_ne!(
            classify_failure(stderr, true).label(),
            WorkerError::DeviceBusy(String::new()).label()
        );
    }

    /// NVDEC/VAAPI's other way of saying the same thing: the decoder itself
    /// could not be set up on this device for this codec. Also a codec answer,
    /// not a health answer — the card keeps encoding fine.
    #[test]
    fn a_failed_hwaccel_decoder_setup_is_a_decode_failure_not_a_device_fault() {
        let stderr = "[h264 @ 0x55] Failed setup for format cuda: \
                      hwaccel initialisation returned error.\n";
        let e = classify_failure(stderr, true);
        assert!(!e.is_transient(), "must not cool the device, got {e:?}");
        assert!(
            matches!(&e, WorkerError::DecodeFailed(w) if w.contains("hwaccel initialisation")),
            "got {e:?}"
        );
    }

    /// The lift must not swallow the hardware arm. A genuine device fault has
    /// no decode wording in it and still has to be transient, or a saturated
    /// card stops being retried elsewhere.
    #[test]
    fn a_real_device_fault_is_still_transient() {
        let e = classify_failure(
            "[h264_nvenc] OpenEncodeSessionEx failed: out of memory",
            true,
        );
        assert!(e.is_transient(), "got {e:?}");
    }

    #[test]
    fn cpu_non_source_failure_is_other() {
        match classify_failure("some weird libx264 explosion", false) {
            WorkerError::Other(s) => assert!(s.contains("explosion")),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    /// Shape of a real burn-in failure: ffmpeg named its reason, then
    /// fontconfig repeated itself until the reason was outside any
    /// fixed-size tail. The reported error must still name the reason.
    #[test]
    fn library_chatter_cannot_push_the_reason_out_of_the_reported_tail() {
        let mut stderr = String::from(
            "[AVFilterGraph] Error initializing filter 'subtitles'\n\
             Error opening filters!\n",
        );
        for _ in 0..40 {
            stderr.push_str(
                "Fontconfig error: No writable cache directories\n\
                 \t/var/cache/fontconfig\n\
                 \t/var/lib/pharos/.cache/fontconfig\n\n",
            );
        }
        match classify_failure(&stderr, false) {
            WorkerError::Other(s) => {
                assert!(
                    s.contains("Error opening filters!"),
                    "reported tail must name the reason, got: {s}"
                );
                assert!(
                    !s.contains("Fontconfig"),
                    "chatter must not fill the window, got: {s}"
                );
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    /// A decisive line further back than the reported window still decides
    /// the class — the quote is truncated, the classification is not.
    #[test]
    fn a_decisive_line_beyond_the_window_still_classifies() {
        let mut stderr = String::from("x.mkv: No such file or directory\n");
        for i in 0..80 {
            stderr.push_str(&format!("[hevc] concealing errors in frame {i}\n"));
        }
        assert!(matches!(
            classify_failure(&stderr, false),
            WorkerError::BadInput(_)
        ));
    }

    #[test]
    fn an_all_noise_dump_still_reports_something() {
        let stderr = "Fontconfig error: No writable cache directories\n\t/var/cache/fontconfig\n";
        match classify_failure(stderr, false) {
            WorkerError::Other(s) => assert!(s.contains("Fontconfig")),
            other => panic!("expected Other, got {other:?}"),
        }
    }
}
