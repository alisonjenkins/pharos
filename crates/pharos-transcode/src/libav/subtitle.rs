//! In-process SubRip (`.srt`) → WebVTT conversion — replaces the
//! `ffmpeg -i sidecar.srt -c:s webvtt -f webvtt` spawn for the sidecar
//! path in `pharos-server::api::jellyfin::subtitles`. This is a pure
//! text transform (SRT and WebVTT cue bodies are identical; only the
//! header and the timestamp decimal separator differ), so it needs no
//! libav and never forks.
//!
//! Embedded-stream extraction (`-map 0:s:<idx> -f webvtt`) requires a
//! subtitle codec round-trip and stays on the spawn path; it is a
//! per-playback op, not a per-scan hotspot.

/// Convert SubRip text to WebVTT. Returns the WebVTT document as bytes.
/// Tolerant of CRLF, a leading BOM, and missing cue indices.
pub fn convert_srt_to_webvtt(srt: &str) -> String {
    let src = srt.strip_prefix('\u{feff}').unwrap_or(srt);
    let mut out = String::with_capacity(src.len() + 16);
    out.push_str("WEBVTT\n\n");

    // Blocks are separated by one or more blank lines.
    let mut first = true;
    for block in src.split("\n\n").flat_map(|b| b.split("\r\n\r\n")) {
        let block = block.trim_matches(['\r', '\n']);
        if block.is_empty() {
            continue;
        }
        let mut lines = block.lines().peekable();

        // Drop a leading numeric SRT cue index (WebVTT cue ids are
        // optional and ffmpeg omits them).
        if let Some(first_line) = lines.peek() {
            if first_line.trim().parse::<u64>().is_ok() {
                lines.next();
            }
        }

        let mut cue = String::new();
        for line in lines {
            if line.contains("-->") {
                cue.push_str(&convert_timestamp_line(line));
            } else {
                cue.push_str(line);
            }
            cue.push('\n');
        }
        let cue = cue.trim_end_matches('\n');
        if cue.is_empty() {
            continue;
        }
        if !first {
            out.push('\n');
        }
        first = false;
        out.push_str(cue);
        out.push('\n');
    }
    out
}

/// SRT uses `HH:MM:SS,mmm`; WebVTT uses `HH:MM:SS.mmm`. Only the
/// millisecond comma changes — leave any cue settings after the timestamp
/// untouched.
fn convert_timestamp_line(line: &str) -> String {
    let mut s = String::with_capacity(line.len());
    let bytes = line.as_bytes();
    for (i, ch) in line.char_indices() {
        // Replace a comma only when flanked by ASCII digits (the ms
        // separator), not commas in cue text.
        if ch == ','
            && i > 0
            && bytes[i - 1].is_ascii_digit()
            && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)
        {
            s.push('.');
        } else {
            s.push(ch);
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_srt_to_vtt() {
        let srt = "1\n00:00:01,000 --> 00:00:04,000\nHello, world\n\n2\n00:00:05,500 --> 00:00:06,000\nSecond line\n";
        let vtt = convert_srt_to_webvtt(srt);
        assert!(vtt.starts_with("WEBVTT\n\n"), "missing header: {vtt:?}");
        assert!(
            vtt.contains("00:00:01.000 --> 00:00:04.000"),
            "ts1: {vtt:?}"
        );
        assert!(
            vtt.contains("00:00:05.500 --> 00:00:06.000"),
            "ts2: {vtt:?}"
        );
        // The comma in "Hello, world" must survive.
        assert!(vtt.contains("Hello, world"), "text comma lost: {vtt:?}");
        // SRT indices dropped.
        assert!(
            !vtt.contains("\n1\n") && !vtt.lines().any(|l| l == "2"),
            "index leaked: {vtt:?}"
        );
    }

    #[test]
    fn crlf_and_bom() {
        let srt = "\u{feff}1\r\n00:00:01,000 --> 00:00:02,000\r\nHi\r\n";
        let vtt = convert_srt_to_webvtt(srt);
        assert!(vtt.starts_with("WEBVTT\n\n"));
        assert!(vtt.contains("00:00:01.000 --> 00:00:02.000"));
        assert!(vtt.contains("Hi"));
    }
}
