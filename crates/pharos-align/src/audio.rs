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
    let bytes = run(
        ffmpeg,
        ffmpeg_args(path, start_ms, end_ms, RATE),
        None,
        timeout,
        (start_ms, end_ms),
    )?;
    Ok(f32s(&bytes))
}

/// The longest song decoded whole for the recogniser: an hour.
pub const SONG_MS: i64 = 3_600_000;

/// The first [`SONG_MS`] of `path` at `rate` in stereo, as the separator
/// takes it.
pub fn decode_stereo(ffmpeg: &str, path: &Path, rate: u32) -> Result<[Vec<f32>; 2], DecodeError> {
    let mut args = ffmpeg_args(path, 0, SONG_MS, rate);
    if let Some(ac) = args.iter().position(|a| a == "-ac") {
        args[ac + 1] = "2".into();
    }
    let bytes = run(ffmpeg, args, None, TIMEOUT, (0, SONG_MS))?;
    let mut out = [Vec::new(), Vec::new()];
    for frame in f32s(&bytes).chunks_exact(2) {
        out[0].push(frame[0]);
        out[1].push(frame[1]);
    }
    Ok(out)
}

/// `stereo` at `from` Hz down to [`RATE`] mono with ffmpeg's resampler: the
/// recogniser and the letter model hear separated vocals as they hear a file.
pub fn resample_mono(
    ffmpeg: &str,
    stereo: &[Vec<f32>; 2],
    from: u32,
) -> Result<Vec<f32>, DecodeError> {
    let mut input = Vec::with_capacity(stereo[0].len() * 8);
    for (l, r) in stereo[0].iter().zip(&stereo[1]) {
        input.extend_from_slice(&l.to_le_bytes());
        input.extend_from_slice(&r.to_le_bytes());
    }
    let len_ms =
        i64::try_from(stereo[0].len() as u64 * 1000 / u64::from(from.max(1))).unwrap_or(i64::MAX);
    let args: Vec<OsString> = ["-nostdin", "-v", "error", "-f", "f32le", "-ac", "2", "-ar"]
        .into_iter()
        .map(OsString::from)
        .chain([from.to_string().into()])
        .chain(["-i", "-", "-ac", "1", "-ar"].map(OsString::from))
        .chain([RATE.to_string().into()])
        .chain(["-f", "f32le", "-"].map(OsString::from))
        .collect();
    let bytes = run(ffmpeg, args, Some(input), TIMEOUT, (0, len_ms))?;
    Ok(f32s(&bytes))
}

fn f32s(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

/// ffmpeg's stdout for `args`, `stdin` fed to it; `window` names the audio
/// in errors.
fn run(
    ffmpeg: &str,
    args: Vec<OsString>,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
    (start_ms, end_ms): (i64, i64),
) -> Result<Vec<u8>, DecodeError> {
    let mut child = Command::new(ffmpeg)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| DecodeError::Spawn {
            program: ffmpeg.to_owned(),
            source,
        })?;
    // Every pipe drains on its own thread: a window's PCM outgrows the
    // pipe buffer, and ffmpeg blocks until it is read.
    let feed = match (stdin, child.stdin.take()) {
        (Some(bytes), Some(mut pipe)) => Some(std::thread::spawn(move || {
            use std::io::Write;
            // A broken pipe means ffmpeg stopped reading; its status says why.
            let _ = pipe.write_all(&bytes);
        })),
        _ => None,
    };
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
    if let Some(feed) = feed {
        let _ = feed.join();
    }
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
    Ok(bytes)
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
    fn a_song_decodes_in_stereo_and_resamples_to_mono_at_the_model_rate() {
        let Some(ffmpeg) = ffmpeg() else {
            eprintln!("ffmpeg not on PATH; skipping");
            return;
        };
        let dir = std::env::temp_dir().join(format!("pharos-align-stereo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("tone.wav");
        let made = Command::new(ffmpeg)
            .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("sine=frequency=440:duration=2:sample_rate=44100")
            .args(["-ac", "2"])
            .arg(&wav)
            .status()
            .unwrap();
        assert!(made.success());
        let stereo = decode_stereo(ffmpeg, &wav, 44_100).unwrap();
        assert_eq!(stereo[0].len(), 88_200);
        assert_eq!(stereo[0], stereo[1], "the tone is the same both sides");
        let mono = resample_mono(ffmpeg, &stereo, 44_100).unwrap();
        assert!((mono.len() as i64 - 32_000).abs() <= 16, "{}", mono.len());
        assert!(mono.iter().any(|s| s.abs() > 0.05));
        let missing = decode_stereo(ffmpeg, &dir.join("missing.wav"), 44_100).unwrap_err();
        assert!(matches!(missing, DecodeError::Failed { .. }), "{missing}");
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
