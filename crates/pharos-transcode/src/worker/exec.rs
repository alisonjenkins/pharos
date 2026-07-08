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

/// Map an ffmpeg failure (stderr text + nonzero exit) to a `WorkerError`
/// the scheduler can route on.
///
/// Classification rules, in order:
/// 1. A **hard source-decode error** (corrupt/missing input, missing
///    decoder) is `BadInput` — non-recoverable; it would fail on every
///    device, so retrying elsewhere is pointless.
/// 2. Otherwise, a failure on a **hardware device** is `DeviceBusy`
///    (transient) so the scheduler falls back to the next device / CPU.
///    Real hardware faults are myriad and version-specific ("Cannot load
///    libcuda.so.1", "no capable devices", VAAPI format-link errors, out
///    of sessions, …); rather than enumerate them, we treat any
///    non-source-error HW failure as a reason to fall back. The CPU is
///    the terminal device, so a genuinely broken job still surfaces a
///    real error there.
/// 3. A failure on the **CPU** device that isn't a source error is
///    `Other` — a genuine, non-recoverable encode error.
pub fn classify_failure(stderr: &str, is_hw: bool) -> WorkerError {
    let s = stderr.to_ascii_lowercase();
    let hard_bad_input = s.contains("invalid data found")
        || s.contains("could not find codec")
        || s.contains("decoder not found")
        || s.contains("no such file")
        || s.contains("unable to find a suitable output format");
    // The stderr tail is the actual ffmpeg reason — carry it on every
    // classification so the log names the cause, never a bare class.
    let tail: String = {
        let chars: Vec<char> = stderr.trim_end().chars().collect();
        let start = chars.len().saturating_sub(400);
        chars[start..].iter().collect()
    };
    if hard_bad_input {
        return WorkerError::BadInput(tail);
    }
    if is_hw {
        return WorkerError::DeviceBusy;
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
            let args = ffmpeg_transcode_args(input, &spec.opts, spec.device, out_str);
            Ok((args, SpawnTarget::File(path.clone())))
        }
        OutputSink::Stdout => {
            let args = ffmpeg_transcode_args(input, &spec.opts, spec.device, "pipe:1");
            Ok((args, SpawnTarget::Stdout))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hw_failure_is_transient_so_it_falls_back() {
        // Real failures seen on a GPU-less box — all must be DeviceBusy
        // when the device is HW, so the scheduler retries on CPU.
        assert_eq!(
            classify_failure("[h264_nvenc] Cannot load libcuda.so.1", true),
            WorkerError::DeviceBusy
        );
        assert_eq!(
            classify_failure("Impossible to convert between the formats", true),
            WorkerError::DeviceBusy
        );
        assert_eq!(
            classify_failure("OpenEncodeSessionEx failed: out of memory", true),
            WorkerError::DeviceBusy
        );
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

    #[test]
    fn cpu_non_source_failure_is_other() {
        match classify_failure("some weird libx264 explosion", false) {
            WorkerError::Other(s) => assert!(s.contains("explosion")),
            other => panic!("expected Other, got {other:?}"),
        }
    }
}
