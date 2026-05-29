//! Live-TV backends (T47).
//!
//! Phase 1 ships one backend: `M3uXmltvBackend` — parses an M3U
//! playlist for channels and (optionally) an XMLTV file for EPG. Both
//! are local files; remote fetching can wrap this with a refresher
//! task later. HDHomeRun support is tracked under T47 phase 2.

use pharos_core::{DomainError, DomainResult, EpgProgram, LiveChannel, TunerBackend};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
pub struct M3uXmltvBackend {
    inner: Arc<RwLock<BackendData>>,
}

#[derive(Debug, Default)]
struct BackendData {
    channels: Vec<LiveChannel>,
    /// EPG indexed by channel id for fast lookup. Each channel's vec
    /// is sorted by `start_unix_ms`.
    epg: HashMap<String, Vec<EpgProgram>>,
}

#[derive(Debug, thiserror::Error)]
pub enum LiveTvError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed m3u: {0}")]
    Malformed(String),
}

impl M3uXmltvBackend {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(BackendData::default())),
        }
    }

    /// Load channels from an M3U file. Replaces any previously loaded
    /// channel set.
    pub async fn load_m3u(&self, path: &Path) -> Result<usize, LiveTvError> {
        let body = tokio::fs::read_to_string(path).await?;
        let channels = parse_m3u(&body)?;
        let n = channels.len();
        let mut g = self.inner.write().await;
        g.channels = channels;
        Ok(n)
    }

    /// Load EPG from an XMLTV file. Replaces any previously loaded EPG.
    pub async fn load_xmltv(&self, path: &Path) -> Result<usize, LiveTvError> {
        let body = tokio::fs::read_to_string(path).await?;
        let programs = parse_xmltv(&body)?;
        let n = programs.len();
        let mut by_channel: HashMap<String, Vec<EpgProgram>> = HashMap::new();
        for p in programs {
            by_channel.entry(p.channel_id.clone()).or_default().push(p);
        }
        for v in by_channel.values_mut() {
            v.sort_by_key(|p| p.start_unix_ms);
        }
        let mut g = self.inner.write().await;
        g.epg = by_channel;
        Ok(n)
    }
}

impl Default for M3uXmltvBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl TunerBackend for M3uXmltvBackend {
    async fn channels(&self) -> DomainResult<Vec<LiveChannel>> {
        Ok(self.inner.read().await.channels.clone())
    }

    async fn programs(
        &self,
        start_unix_ms: u64,
        end_unix_ms: u64,
    ) -> DomainResult<Vec<EpgProgram>> {
        if start_unix_ms >= end_unix_ms {
            return Err(DomainError::Backend("epg window: start >= end".into()));
        }
        let g = self.inner.read().await;
        let mut out = Vec::new();
        for programs in g.epg.values() {
            // EPG is sorted by start; collect entries whose window
            // overlaps the requested span.
            for p in programs {
                if p.end_unix_ms <= start_unix_ms {
                    continue;
                }
                if p.start_unix_ms >= end_unix_ms {
                    break;
                }
                out.push(p.clone());
            }
        }
        Ok(out)
    }
}

/// Parse a standard `#EXTM3U` playlist. Each channel is a pair of
/// lines: `#EXTINF:-1 tvg-id="…" tvg-logo="…" group-title="…",Channel Name`
/// followed by the stream URL.
pub fn parse_m3u(body: &str) -> Result<Vec<LiveChannel>, LiveTvError> {
    let mut out = Vec::new();
    let mut pending: Option<ExtinfMeta> = None;
    let mut next_number: u32 = 1;
    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() || line == "#EXTM3U" || line.starts_with("#EXTM3U:") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            pending = Some(parse_extinf(rest));
            continue;
        }
        if line.starts_with('#') {
            // Other directives (#EXTVLCOPT, #EXTGRP, etc.) — skip.
            continue;
        }
        let meta = pending
            .take()
            .ok_or_else(|| LiveTvError::Malformed(format!("stream URL with no #EXTINF: {line}")))?;
        let id = meta
            .tvg_id
            .clone()
            .unwrap_or_else(|| format!("ch-{next_number}"));
        let number = meta
            .tvg_chno
            .clone()
            .unwrap_or_else(|| next_number.to_string());
        out.push(LiveChannel {
            id,
            number,
            name: meta.name,
            logo_url: meta.tvg_logo,
            stream_url: line.to_string(),
            group_title: meta.group_title,
        });
        next_number += 1;
    }
    Ok(out)
}

#[derive(Debug, Default)]
struct ExtinfMeta {
    name: String,
    tvg_id: Option<String>,
    tvg_logo: Option<String>,
    tvg_chno: Option<String>,
    group_title: Option<String>,
}

fn parse_extinf(rest: &str) -> ExtinfMeta {
    // Shape: `-1 key="value" key2="value 2",Channel Name`
    // Split at the LAST comma — the channel name follows.
    let (attrs, name) = match rest.rsplit_once(',') {
        Some((a, n)) => (a.trim(), n.trim().to_string()),
        None => (rest.trim(), String::new()),
    };
    let mut meta = ExtinfMeta {
        name,
        ..ExtinfMeta::default()
    };
    let mut chars = attrs.chars().peekable();
    let mut tok = String::new();
    while let Some(c) = chars.next() {
        if c == '"' {
            // Read until matching close-quote.
            let mut v = String::new();
            for c2 in chars.by_ref() {
                if c2 == '"' {
                    break;
                }
                v.push(c2);
            }
            // `tok` is the buffer accumulated since the last quoted
            // value. The key is the trailing whitespace-delimited
            // identifier ending in `=` — anything before is leftover
            // (`-1`, the previous value's neighbours, etc.).
            let trimmed = tok.trim_end_matches(|c: char| c.is_whitespace() || c == '=');
            // The key is the trailing whitespace-delimited token. Use
            // `rsplit` rather than byte-index slicing so a multi-byte
            // Unicode whitespace separator (e.g. NBSP) can't land us on a
            // non-char-boundary and panic.
            let key = trimmed
                .rsplit(char::is_whitespace)
                .next()
                .unwrap_or(trimmed)
                .to_string();
            let lower = key.to_ascii_lowercase();
            match lower.as_str() {
                "tvg-id" => meta.tvg_id = Some(v),
                "tvg-logo" => meta.tvg_logo = Some(v),
                "tvg-chno" => meta.tvg_chno = Some(v),
                "group-title" => meta.group_title = Some(v),
                _ => {}
            }
            tok.clear();
        } else {
            tok.push(c);
        }
    }
    meta
}

/// Parse an XMLTV file. Handles the common `<programme channel="…"
/// start="20240101T103000 +0000" stop="…"><title>…</title>
/// <desc>…</desc></programme>` shape via a small hand-rolled scanner —
/// keeps the dep budget low (no full XML parser pulled in just for
/// EPG).
pub fn parse_xmltv(body: &str) -> Result<Vec<EpgProgram>, LiveTvError> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while let Some(start) = find_subslice(bytes, b"<programme", i) {
        // Find the end of the open tag.
        let Some(tag_end) = find_subslice(bytes, b">", start) else {
            break;
        };
        let open = &body[start..tag_end];
        let channel = attr_value(open, "channel").unwrap_or_default();
        let start_raw = attr_value(open, "start").unwrap_or_default();
        let stop_raw = attr_value(open, "stop").unwrap_or_default();

        let Some(prog_close) = find_subslice(bytes, b"</programme>", tag_end) else {
            break;
        };
        let body_inner = &body[tag_end + 1..prog_close];
        let title = extract_tag_text(body_inner, "title").unwrap_or_default();
        let description = extract_tag_text(body_inner, "desc");

        let Some(start_ms) = parse_xmltv_time_ms(&start_raw) else {
            i = prog_close + b"</programme>".len();
            continue;
        };
        let Some(end_ms) = parse_xmltv_time_ms(&stop_raw) else {
            i = prog_close + b"</programme>".len();
            continue;
        };
        out.push(EpgProgram {
            channel_id: channel,
            title,
            description,
            start_unix_ms: start_ms,
            end_unix_ms: end_ms,
        });
        i = prog_close + b"</programme>".len();
    }
    Ok(out)
}

fn find_subslice(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from > hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

fn attr_value(tag: &str, attr: &str) -> Option<String> {
    let pat = format!("{attr}=\"");
    let i = tag.find(&pat)?;
    let after = &tag[i + pat.len()..];
    let end = after.find('"')?;
    Some(after[..end].to_string())
}

fn extract_tag_text(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let i = body.find(&open)?;
    let after_open = &body[i + open.len()..];
    let gt = after_open.find('>')?;
    let inner_start = i + open.len() + gt + 1;
    let inner = &body[inner_start..];
    let end = inner.find(&close)?;
    Some(inner[..end].trim().to_string())
}

/// XMLTV time format: `YYYYMMDDhhmmss [+-]HHMM`. The timezone offset
/// is optional but typically present.
fn parse_xmltv_time_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    // The attribute value is fully input-controlled (third-party EPG
    // files). Operate on bytes via `get` so a multi-byte UTF-8 char that
    // straddles a fixed index returns None instead of panicking on a
    // non-char-boundary slice.
    let b = s.as_bytes();
    let field = |range: std::ops::Range<usize>| -> Option<&str> {
        s.get(range).filter(|x| x.is_ascii())
    };
    let year: i64 = field(0..4)?.parse().ok()?;
    let month: u32 = field(4..6)?.parse().ok()?;
    let day: u32 = field(6..8)?.parse().ok()?;
    let hour: u32 = field(8..10)?.parse().ok()?;
    let minute: u32 = field(10..12)?.parse().ok()?;
    let second: u32 = field(12..14)?.parse().ok()?;
    let offset_minutes: i64 = if s.len() >= 20 {
        let sign = match b.get(15)? {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let hh: i64 = field(16..18)?.parse().ok()?;
        let mm: i64 = field(18..20)?.parse().ok()?;
        sign * (hh * 60 + mm)
    } else {
        0
    };
    let unix_secs = ymd_hms_to_unix(year, month, day, hour, minute, second)?;
    let with_offset = unix_secs.checked_sub(offset_minutes * 60)?;
    Some((with_offset as u64).saturating_mul(1000))
}

/// Civil-from-days algorithm (Howard Hinnant) — yields a unix-seconds
/// timestamp from a Y-M-D + h:m:s. Returns None on date arithmetic
/// overflow; otherwise infallible.
fn ymd_hms_to_unix(y: i64, m: u32, d: u32, h: u32, minute: u32, s: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m as i64 - 3 } else { m as i64 + 9 };
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs_of_day = (h as i64) * 3600 + (minute as i64) * 60 + (s as i64);
    days.checked_mul(86_400)?.checked_add(secs_of_day)
}

/// Read a backend config: either `live_tv = { m3u = "...", xmltv = "..." }`
/// or absent. Lives here so `serve` can build the backend at startup
/// without duplicating the parsing.
pub async fn build_backend(
    m3u: Option<PathBuf>,
    xmltv: Option<PathBuf>,
) -> Result<Option<M3uXmltvBackend>, LiveTvError> {
    let Some(m3u) = m3u else {
        return Ok(None);
    };
    let backend = M3uXmltvBackend::new();
    backend.load_m3u(&m3u).await?;
    if let Some(xmltv) = xmltv {
        backend.load_xmltv(&xmltv).await?;
    }
    Ok(Some(backend))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn xmltv_time_multibyte_does_not_panic() {
        // Multi-byte char straddling a fixed index must yield None, not
        // panic. (é is 2 bytes; placing it inside the field region used
        // to slice on a non-char boundary.)
        assert!(parse_xmltv_time_ms("20é40101000000").is_none());
        assert!(parse_xmltv_time_ms("2024010100000é +0000").is_none());
        // Valid input still parses.
        assert!(parse_xmltv_time_ms("20240101103000 +0000").is_some());
    }

    #[test]
    fn extinf_nbsp_separator_does_not_panic() {
        // A non-breaking space (U+00A0, 2 bytes) before a key used to
        // panic on a non-char-boundary slice. Must parse without panic.
        let line = "#EXTINF:-1\u{a0}tvg-id=\"x\" group-title=\"G\",Name";
        let m3u = format!("#EXTM3U\n{line}\nhttp://e/x.ts\n");
        let chans = parse_m3u(&m3u).unwrap();
        assert_eq!(chans.len(), 1);
    }

    const SAMPLE_M3U: &str = r#"#EXTM3U
#EXTINF:-1 tvg-id="bbc1" tvg-logo="https://example/bbc.png" tvg-chno="1" group-title="UK",BBC One
http://example/bbc1.ts
#EXTINF:-1 tvg-id="cnn" group-title="News",CNN
http://example/cnn.ts
"#;

    const SAMPLE_XMLTV: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<tv>
  <channel id="bbc1"><display-name>BBC One</display-name></channel>
  <programme channel="bbc1" start="20240101100000 +0000" stop="20240101110000 +0000">
    <title>News at Ten</title>
    <desc>Top stories.</desc>
  </programme>
  <programme channel="cnn" start="20240101110000 +0000" stop="20240101113000 +0000">
    <title>CNN Hour</title>
  </programme>
</tv>
"#;

    #[test]
    fn m3u_parses_two_channels_with_metadata() {
        let chs = parse_m3u(SAMPLE_M3U).unwrap();
        assert_eq!(chs.len(), 2);
        assert_eq!(chs[0].id, "bbc1");
        assert_eq!(chs[0].number, "1");
        assert_eq!(chs[0].name, "BBC One");
        assert_eq!(chs[0].logo_url.as_deref(), Some("https://example/bbc.png"));
        assert_eq!(chs[0].group_title.as_deref(), Some("UK"));
        assert_eq!(chs[0].stream_url, "http://example/bbc1.ts");
        assert_eq!(chs[1].id, "cnn");
        // Default channel number from sequence.
        assert_eq!(chs[1].number, "2");
    }

    #[test]
    fn m3u_dangling_url_with_no_extinf_errors() {
        let bad = "#EXTM3U\nhttp://example/bare\n";
        assert!(parse_m3u(bad).is_err());
    }

    #[test]
    fn xmltv_parses_programmes_with_times_in_utc() {
        let programs = parse_xmltv(SAMPLE_XMLTV).unwrap();
        assert_eq!(programs.len(), 2);
        assert_eq!(programs[0].channel_id, "bbc1");
        assert_eq!(programs[0].title, "News at Ten");
        assert_eq!(programs[0].description.as_deref(), Some("Top stories."));
        // 2024-01-01 10:00:00 UTC = 1704103200
        assert_eq!(programs[0].start_unix_ms, 1_704_103_200_000);
        assert_eq!(programs[0].end_unix_ms, 1_704_106_800_000);
    }

    #[tokio::test]
    async fn backend_returns_channels_and_filters_epg_to_window() {
        let backend = M3uXmltvBackend::new();
        // Write fixtures.
        let td = tempfile::TempDir::new().unwrap();
        let m3u_path = td.path().join("p.m3u");
        let xml_path = td.path().join("epg.xml");
        tokio::fs::write(&m3u_path, SAMPLE_M3U).await.unwrap();
        tokio::fs::write(&xml_path, SAMPLE_XMLTV).await.unwrap();
        assert_eq!(backend.load_m3u(&m3u_path).await.unwrap(), 2);
        assert_eq!(backend.load_xmltv(&xml_path).await.unwrap(), 2);
        let chs = backend.channels().await.unwrap();
        assert_eq!(chs.len(), 2);
        // Window covers only the bbc1 entry (10:00-11:00 UTC).
        let programs = backend
            .programs(1_704_103_000_000, 1_704_106_000_000)
            .await
            .unwrap();
        assert_eq!(programs.len(), 1);
        assert_eq!(programs[0].title, "News at Ten");
    }

    #[tokio::test]
    async fn invalid_epg_window_errors() {
        let backend = M3uXmltvBackend::new();
        let res = backend.programs(100, 100).await;
        assert!(res.is_err());
    }
}
