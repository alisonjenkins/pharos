//! Audio windows decoded by an ffmpeg subprocess, as `pharos_aligner.audio` does it.

use std::ffi::OsString;
use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use wait_timeout::ChildExt;

/// The model's input rate.
pub const RATE: u32 = 16_000;
/// The Python service's limit for one window; a hung ffmpeg is killed after it.
pub const TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("could not start {program}: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("ffmpeg exited with {status} for {start_ms}..{end_ms} ms: {stderr}")]
    Failed {
        status: ExitStatus,
        start_ms: i64,
        end_ms: i64,
        stderr: String,
    },
    #[error("ffmpeg took over {}s for {start_ms}..{end_ms} ms", .after.as_secs())]
    TimedOut {
        after: Duration,
        start_ms: i64,
        end_ms: i64,
    },
    #[error("reading ffmpeg output: {0}")]
    Io(#[from] std::io::Error),
}

/// ffmpeg's arguments for mono f32le at `rate` from `start_ms` to `end_ms`,
/// seconds printed to the millisecond as the Python's `"%.3f"`.
pub fn ffmpeg_args(path: &Path, start_ms: i64, end_ms: i64, rate: u32) -> Vec<OsString> {
    let secs = |ms: i64| format!("{:.3}", ms as f64 / 1000.0);
    let mut args: Vec<OsString> = ["-nostdin", "-v", "error", "-ss"]
        .map(OsString::from)
        .into();
    args.push(secs(start_ms).into());
    args.push("-t".into());
    args.push(secs(end_ms - start_ms).into());
    args.push("-i".into());
    args.push(path.as_os_str().to_owned());
    for a in ["-ac", "1", "-ar"] {
        args.push(a.into());
    }
    args.push(rate.to_string().into());
    for a in ["-f", "f32le", "-"] {
        args.push(a.into());
    }
    args
}

/// Decodes `start_ms..end_ms` of `path` to 16 kHz mono samples with `ffmpeg`.
pub fn decode(
    ffmpeg: &str,
    path: &Path,
    start_ms: i64,
    end_ms: i64,
) -> Result<Vec<f32>, DecodeError> {
    decode_with(ffmpeg, path, start_ms, end_ms, TIMEOUT)
}

fn decode_with(
    ffmpeg: &str,
    path: &Path,
    start_ms: i64,
    end_ms: i64,
    timeout: Duration,
) -> Result<Vec<f32>, DecodeError> {
    let mut child = Command::new(ffmpeg)
        .args(ffmpeg_args(path, start_ms, end_ms, RATE))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| DecodeError::Spawn {
            program: ffmpeg.to_owned(),
            source,
        })?;
    // Both pipes drain on their own threads: a window's PCM outgrows the
    // pipe buffer, and ffmpeg blocks until it is read.
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("no stdout pipe"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("no stderr pipe"))?;
    let out = std::thread::spawn(move || {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).map(|_| buf)
    });
    let err = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let Some(status) = child.wait_timeout(timeout)? else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(DecodeError::TimedOut {
            after: timeout,
            start_ms,
            end_ms,
        });
    };
    let bytes = out
        .join()
        .map_err(|_| std::io::Error::other("stdout reader panicked"))??;
    let stderr = err.join().unwrap_or_default();
    if !status.success() {
        return Err(DecodeError::Failed {
            status,
            start_ms,
            end_ms,
            stderr: String::from_utf8_lossy(&stderr).trim().to_owned(),
        });
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn args_match_audio_py() {
        let got: Vec<String> = ffmpeg_args(Path::new("/m/a.flac"), 1234, 5678, RATE)
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        let want = "-nostdin -v error -ss 1.234 -t 4.444 -i /m/a.flac -ac 1 -ar 16000 -f f32le -";
        assert_eq!(got.join(" "), want);
        let zero = ffmpeg_args(Path::new("a"), 0, 250, RATE);
        assert_eq!(
            (zero[4].to_str(), zero[6].to_str()),
            (Some("0.000"), Some("0.250"))
        );
    }

    fn ffmpeg() -> Option<&'static str> {
        Command::new("ffmpeg")
            .arg("-version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|_| "ffmpeg")
    }

    #[test]
    fn decodes_a_window_and_reports_failures() {
        let Some(ffmpeg) = ffmpeg() else {
            eprintln!("ffmpeg not on PATH; skipping");
            return;
        };
        let dir = std::env::temp_dir().join(format!("pharos-align-audio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("tone.wav");
        let made = Command::new(ffmpeg)
            .args([
                "-nostdin",
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=3",
            ])
            .arg(&wav)
            .status()
            .unwrap();
        assert!(made.success());

        let pcm = decode(ffmpeg, &wav, 500, 2000).unwrap();
        assert_eq!(pcm.len(), 24_000);
        assert!(pcm.iter().any(|s| s.abs() > 0.05));

        let missing = decode(ffmpeg, &dir.join("missing.wav"), 0, 1000).unwrap_err();
        assert!(matches!(missing, DecodeError::Failed { .. }), "{missing}");
        let spawn = decode("pharos-no-such-ffmpeg", &wav, 0, 1000).unwrap_err();
        assert!(matches!(spawn, DecodeError::Spawn { .. }), "{spawn}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_hung_decoder_is_killed_at_the_timeout() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pharos-align-hang-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("ffmpeg");
        std::fs::write(&fake, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = std::time::Instant::now();
        let got = decode_with(
            fake.to_str().unwrap(),
            Path::new("a"),
            0,
            1000,
            Duration::from_millis(200),
        );
        assert!(matches!(got, Err(DecodeError::TimedOut { .. })), "{got:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
