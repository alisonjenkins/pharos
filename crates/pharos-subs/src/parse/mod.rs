//! Subtitle input: format sniffing, decoding, and the per-format parsers.

mod ass;
mod cue;
mod tags;

use std::borrow::Cow;

use crate::model::Script;

impl Script {
    /// Parses text in any supported format, sniffed from its content.
    pub fn parse(src: &str) -> Script {
        let src = src.strip_prefix('\u{feff}').unwrap_or(src);
        let head = src.trim_start();
        if head.starts_with("WEBVTT") {
            cue::parse_vtt(src)
        } else if src.contains("[Script Info]") || src.contains("[Events]") {
            ass::parse(src)
        } else {
            cue::parse_srt(src)
        }
    }

    /// Decodes `bytes` then parses. See [`decode`].
    pub fn parse_bytes(bytes: &[u8], label: Option<&str>) -> Script {
        Script::parse(&decode(bytes, label))
    }
}

/// Decodes subtitle bytes to text: BOM-marked UTF-8/16, then valid UTF-8,
/// then (feature `legacy-encodings`) the codepage named by `label`
/// (default windows-1252). Without the feature invalid input is decoded lossily.
pub fn decode<'a>(bytes: &'a [u8], label: Option<&str>) -> Cow<'a, str> {
    #[cfg(feature = "legacy-encodings")]
    {
        if let Some((enc, _)) = encoding_rs::Encoding::for_bom(bytes) {
            return enc.decode(bytes).0;
        }
        if let Ok(s) = std::str::from_utf8(bytes) {
            return Cow::Borrowed(s);
        }
        let enc = label
            .and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()))
            .unwrap_or(encoding_rs::WINDOWS_1252);
        enc.decode(bytes).0
    }
    #[cfg(not(feature = "legacy-encodings"))]
    {
        let _ = label;
        String::from_utf8_lossy(bytes)
    }
}

#[cfg(test)]
mod tests;
