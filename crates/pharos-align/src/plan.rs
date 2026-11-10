//! Request parsing, per-line decode windows and word stamps, as
//! `pharos_aligner.plan` does them.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::text::Word;

/// Audio kept past the end of a line and, at least, ahead of its start.
pub const MARGIN_MS: i64 = 250;
/// Synced lyrics are mostly tapped by ear, so a stamp often lands a few
/// hundred ms after the singing starts; a window opening only `MARGIN_MS`
/// early cuts off the first word, and CTC then pins it late.
pub const LEAD_MS: i64 = 750;
/// A line is aligned against at most this much audio a word plus
/// `CAP_EXTRA_MS`, so a line before an instrumental break is not handed a
/// window that is mostly music.
pub const CAP_MS_PER_WORD: i64 = 700;
pub const CAP_EXTRA_MS: i64 = 2000;
/// Request limits; the server rejects larger bodies with a 400.
pub const MAX_LINES: usize = 500;
pub const MAX_TEXT_CHARS: usize = 500;
/// Latest `start_ms` or `end_ms` a request may carry: about 31 years, past
/// any song, and far enough below `i64::MAX` that the window and probe
/// margins added to it cannot overflow.
pub const MAX_TIMESTAMP_MS: i64 = 1_000_000_000_000;

/// One synced lyric line as the request gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

/// A word's start, or the line's end when `at` is the line length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Stamp {
    /// Code-point offset into the line text.
    pub at: usize,
    pub start_ms: i64,
}

/// Why a request was rejected; the messages are the Python service's, word for word.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    #[error("request body must be an object")]
    NotObject,
    #[error("path is required")]
    NoPath,
    #[error("path must be a string")]
    PathNotString,
    #[error("path cannot be empty")]
    EmptyPath,
    #[error("lines is required")]
    NoLines,
    #[error("lines must be a list")]
    LinesNotList,
    #[error("lines cannot exceed 500")]
    TooManyLines,
    #[error("each line must be an object")]
    LineNotObject,
    #[error("line field {0} is required")]
    MissingField(&'static str),
    #[error("{0} must be an integer")]
    NotInteger(&'static str),
    #[error("text must be a string")]
    TextNotString,
    #[error("{0} cannot be negative")]
    Negative(&'static str),
    #[error("{field} cannot exceed {MAX_TIMESTAMP_MS} ms, got {value}")]
    TooLate { field: &'static str, value: String },
    #[error("end_ms cannot be less than start_ms")]
    EndBeforeStart,
    #[error("text cannot exceed 500 characters")]
    TextTooLong,
    #[error("path escapes the media root")]
    Escapes,
}

/// Audio `(start_ms, end_ms)` to decode for `line`, which has `n_words` words.
///
/// `prev_end_ms` is where the line before was aligned to end: the lead stops
/// there, or a line sung straight after another would take its last word.
pub fn window(line: &Line, n_words: usize, prev_end_ms: i64) -> (i64, i64) {
    let words = i64::try_from(n_words).unwrap_or(i64::MAX);
    let cap = line
        .start_ms
        .saturating_add(words.saturating_mul(CAP_MS_PER_WORD))
        .saturating_add(CAP_EXTRA_MS);
    let end_ms = line.end_ms.min(cap);
    let start_ms = (line.start_ms - LEAD_MS).max(prev_end_ms.min(line.start_ms - MARGIN_MS));
    (start_ms.max(0), end_ms + MARGIN_MS)
}

/// Window-relative token spans to absolute, non-decreasing word stamps plus
/// an end stamp at the line's length; empty when the spans do not match the words.
pub fn stamps(
    text: &str,
    aligned: &[Word],
    spans: &[(u64, u64)],
    window_start_ms: i64,
) -> Vec<Stamp> {
    let Some(&(_, last_end)) = spans.last() else {
        return Vec::new();
    };
    if aligned.is_empty() || aligned.len() != spans.len() {
        return Vec::new();
    }
    let at_ms = |rel: u64| window_start_ms.saturating_add(i64::try_from(rel).unwrap_or(i64::MAX));
    let mut out = Vec::with_capacity(aligned.len() + 1);
    let mut last_t = i64::MIN;
    for (w, &(s, _)) in aligned.iter().zip(spans) {
        let t = at_ms(s).max(last_t);
        out.push(Stamp {
            at: w.at,
            start_ms: t,
        });
        last_t = t;
    }
    out.push(Stamp {
        at: text.chars().count(),
        start_ms: at_ms(last_end).max(last_t),
    });
    out
}

/// Python's `isinstance(v, int) and not isinstance(v, bool)` on a JSON value.
fn int_field(v: &Value, field: &'static str) -> Result<i64, RequestError> {
    match v {
        Value::Number(n) if n.is_i64() || n.is_u64() => match n.as_i64() {
            Some(ms) if ms > MAX_TIMESTAMP_MS => Err(RequestError::TooLate {
                field,
                value: ms.to_string(),
            }),
            Some(ms) => Ok(ms),
            // Past i64::MAX: named as sent, not as the i64 it would clamp to.
            None => Err(RequestError::TooLate {
                field,
                value: n.to_string(),
            }),
        },
        _ => Err(RequestError::NotInteger(field)),
    }
}

/// Validates an `/align` body into its media path and lines, checking in the
/// same order as the Python service so the same body gets the same message.
pub fn parse_request(body: &Value) -> Result<(String, Vec<Line>), RequestError> {
    let body = body.as_object().ok_or(RequestError::NotObject)?;
    let path = body.get("path").ok_or(RequestError::NoPath)?;
    let path = path.as_str().ok_or(RequestError::PathNotString)?;
    if path.is_empty() {
        return Err(RequestError::EmptyPath);
    }
    let raw = body.get("lines").ok_or(RequestError::NoLines)?;
    let raw = raw.as_array().ok_or(RequestError::LinesNotList)?;
    if raw.len() > MAX_LINES {
        return Err(RequestError::TooManyLines);
    }
    let mut lines = Vec::with_capacity(raw.len());
    for line in raw {
        let line = line.as_object().ok_or(RequestError::LineNotObject)?;
        let field = |name: &'static str| line.get(name).ok_or(RequestError::MissingField(name));
        let (start, end, text) = (field("start_ms")?, field("end_ms")?, field("text")?);
        let start_ms = int_field(start, "start_ms")?;
        let end_ms = int_field(end, "end_ms")?;
        let text = text.as_str().ok_or(RequestError::TextNotString)?;
        if start_ms < 0 {
            return Err(RequestError::Negative("start_ms"));
        }
        if end_ms < 0 {
            return Err(RequestError::Negative("end_ms"));
        }
        if end_ms < start_ms {
            return Err(RequestError::EndBeforeStart);
        }
        if text.chars().count() > MAX_TEXT_CHARS {
            return Err(RequestError::TextTooLong);
        }
        lines.push(Line {
            start_ms,
            end_ms,
            text: text.to_owned(),
        });
    }
    Ok((path.to_owned(), lines))
}

/// Symlinks followed before a path is given up on, as Linux's `ELOOP` limit.
const MAX_SYMLINKS: usize = 40;

/// `os.path.realpath(path)` without `strict`: symlinks are followed where the
/// path exists, the rest is kept as written, and `..` drops the component
/// before it once that is resolved. `None` on a symlink loop.
fn realpath(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::from("/");
    let mut pending: Vec<std::ffi::OsString> = Vec::new();
    let push_rev = |pending: &mut Vec<std::ffi::OsString>, p: &Path| {
        for c in p.components().rev() {
            pending.push(c.as_os_str().to_owned());
        }
    };
    let cwd;
    let start = if path.is_absolute() {
        path
    } else {
        cwd = std::env::current_dir().ok()?.join(path);
        cwd.as_path()
    };
    push_rev(&mut pending, start);
    let mut links = 0;
    while let Some(part) = pending.pop() {
        match Path::new(&part).components().next() {
            Some(Component::RootDir) => out = PathBuf::from("/"),
            Some(Component::ParentDir) => {
                out.pop();
            }
            Some(Component::Normal(name)) => {
                let next = out.join(name);
                match std::fs::read_link(&next) {
                    Ok(target) => {
                        links += 1;
                        if links > MAX_SYMLINKS {
                            return None;
                        }
                        push_rev(&mut pending, &target);
                    }
                    Err(_) => out = next,
                }
            }
            Some(Component::CurDir | Component::Prefix(_)) | None => {}
        }
    }
    Some(out)
}

/// `path` resolved inside `media_root`, refusing absolute paths, `..` and
/// symlinks that lead out of it, and the root itself.
pub fn resolve(media_root: &Path, path: &str) -> Result<PathBuf, RequestError> {
    let rel = Path::new(path);
    if rel.is_absolute() {
        return Err(RequestError::Escapes);
    }
    let root = realpath(media_root).ok_or(RequestError::Escapes)?;
    let full = realpath(&root.join(rel)).ok_or(RequestError::Escapes)?;
    if full == root || !full.starts_with(&root) {
        return Err(RequestError::Escapes);
    }
    Ok(full)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_timestamp_past_the_limit_is_a_request_error_naming_it() {
        let body = |start: serde_json::Value, end: serde_json::Value| {
            serde_json::json!({"path": "a.mp3", "lines": [
                {"start_ms": start, "end_ms": end, "text": "la la"}
            ]})
        };
        let huge = i64::MAX;
        let err = |b| parse_request(&b).unwrap_err().to_string();
        assert_eq!(
            err(body(huge.into(), huge.into())),
            format!("start_ms cannot exceed 1000000000000 ms, got {huge}")
        );
        assert_eq!(
            err(body(0.into(), (MAX_TIMESTAMP_MS + 1).into())),
            "end_ms cannot exceed 1000000000000 ms, got 1000000000001"
        );
        assert_eq!(
            err(body(0.into(), u64::MAX.into())),
            format!("end_ms cannot exceed 1000000000000 ms, got {}", u64::MAX)
        );
    }

    /// Lines at the limit go through the line window and the offset probe
    /// without overflowing; test builds panic on overflow.
    #[test]
    fn lines_at_the_limit_plan_their_windows_and_probes() {
        let max = MAX_TIMESTAMP_MS;
        let body = serde_json::json!({"path": "a.mp3", "lines": [
            {"start_ms": max - 3_000, "end_ms": max - 2_000, "text": "one two three"},
            {"start_ms": max - 2_000, "end_ms": max - 1_000, "text": "four five six"},
            {"start_ms": max - 1_000, "end_ms": max, "text": "seven eight nine"},
        ]});
        let (_, lines) = parse_request(&body).unwrap();
        let (start, end) = window(&lines[2], 3, lines[1].end_ms);
        assert!(start < end && end > max, "{start}..{end}");
        let mut decoded = Vec::new();
        let Ok(est) = crate::offset::estimate(
            &lines,
            |s, e| {
                decoded.push((s, e));
                Ok::<_, std::convert::Infallible>(Vec::new())
            },
            |_| Ok(Vec::new()),
        );
        assert!(!decoded.is_empty() && decoded.iter().all(|&(s, e)| s < e && e > max));
        assert_eq!(est.offset_ms, 0);
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pharos-align-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("media/dir")).unwrap();
        std::fs::write(d.join("media/dir/song.flac"), b"x").unwrap();
        std::fs::write(d.join("outside.flac"), b"x").unwrap();
        d
    }

    #[test]
    fn resolve_keeps_a_file_inside_the_root() {
        let d = tmp("inside");
        let root = d.join("media");
        let got = resolve(&root, "dir/song.flac").unwrap();
        assert_eq!(
            got,
            std::fs::canonicalize(root.join("dir/song.flac")).unwrap()
        );
        assert_eq!(resolve(&root, "dir/../dir/song.flac").unwrap(), got);
        // A missing file still resolves; opening it is ffmpeg's error.
        assert!(resolve(&root, "dir/missing.flac").is_ok());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn resolve_refuses_escapes() {
        let d = tmp("escape");
        let root = d.join("media");
        for bad in [
            "../outside.flac",
            "/etc/passwd",
            ".",
            "dir/..",
            "dir/../../outside.flac",
        ] {
            assert_eq!(resolve(&root, bad), Err(RequestError::Escapes), "{bad}");
        }
        std::os::unix::fs::symlink(d.join("outside.flac"), root.join("link.flac")).unwrap();
        assert_eq!(resolve(&root, "link.flac"), Err(RequestError::Escapes));
        std::os::unix::fs::symlink("dir/song.flac", root.join("inner.flac")).unwrap();
        assert!(resolve(&root, "inner.flac")
            .unwrap()
            .ends_with("media/dir/song.flac"));
        std::os::unix::fs::symlink("loop", root.join("loop")).unwrap();
        assert_eq!(resolve(&root, "loop"), Err(RequestError::Escapes));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn resolve_follows_a_symlinked_root() {
        let d = tmp("rootlink");
        std::os::unix::fs::symlink(d.join("media"), d.join("alias")).unwrap();
        let got = resolve(&d.join("alias"), "dir/song.flac").unwrap();
        assert_eq!(
            got,
            std::fs::canonicalize(d.join("media/dir/song.flac")).unwrap()
        );
        std::fs::remove_dir_all(d).unwrap();
    }
}
