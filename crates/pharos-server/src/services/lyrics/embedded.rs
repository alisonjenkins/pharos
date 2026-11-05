//! Lyrics in a song's own tags, read with `lofty`; the only module that
//! imports it (009 §Boundaries). An ID3v2 `SYLT` frame is synced lyrics; the
//! text tags (`USLT`, Vorbis `LYRICS`, MP4 `©lyr`, APE `Lyrics`) go through
//! the LRC parser, since taggers often store LRC there.

use std::path::Path;

use lofty::config::ParseOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::id3::v2::{Frame, Id3v2Tag, SynchronizedTextFrame, TimestampFormat};
use lofty::mpeg::MpegFile;
use lofty::tag::ItemKey;

use super::{lrc, LyricLine, Lyrics, Origin};

pub fn read(media: &Path) -> Option<Lyrics> {
    synced(media).or_else(|| text(media))
}

/// Tags only: the audio properties cost a scan of the stream and go unused.
fn options() -> ParseOptions {
    ParseOptions::new().read_properties(false)
}

fn text(media: &Path) -> Option<Lyrics> {
    let read = lofty::probe::Probe::open(media).and_then(|p| p.options(options()).read());
    let tagged = match read {
        Ok(tagged) => tagged,
        Err(e) => {
            tracing::debug!(path = %media.display(), error = %e, "lyrics: tags unreadable");
            return None;
        }
    };
    let words = tagged.tags().iter().find_map(|tag| {
        [ItemKey::Lyrics, ItemKey::UnsyncLyrics]
            .into_iter()
            .find_map(|key| tag.get_string(key).filter(|s| !s.trim().is_empty()))
            .map(str::to_owned)
    })?;
    let lines = lrc::parse(&words);
    let origin = if lines.iter().all(|l| l.start_ms.is_some()) {
        Origin::EmbeddedSynced
    } else {
        Origin::EmbeddedText
    };
    Lyrics::from_lines(lines, origin)
}

/// A `SYLT` frame timed in milliseconds. Frames timed in MPEG frames are
/// skipped: they need the stream's frame rate and almost no tagger writes them.
fn synced(media: &Path) -> Option<Lyrics> {
    let ext = media.extension()?.to_str()?;
    if !ext.eq_ignore_ascii_case("mp3") {
        return None;
    }
    let mut file = std::fs::File::open(media).ok()?;
    let mpeg = MpegFile::read_from(&mut file, options()).ok()?;
    sylt(mpeg.id3v2()?)
}

fn sylt(tag: &Id3v2Tag) -> Option<Lyrics> {
    tag.into_iter().find_map(|frame| {
        if frame.id_str() != "SYLT" {
            return None;
        }
        let Frame::Binary(binary) = frame else {
            return None;
        };
        let parsed = match SynchronizedTextFrame::parse(&binary.data, binary.flags()) {
            Ok(parsed) => parsed,
            Err(e) => {
                tracing::debug!(error = %e, "lyrics: SYLT frame unreadable");
                return None;
            }
        };
        if parsed.timestamp_format != TimestampFormat::MS {
            return None;
        }
        let mut lines: Vec<LyricLine> = parsed
            .content
            .into_iter()
            .map(|(at, text)| LyricLine {
                start_ms: Some(u64::from(at)),
                text: text.trim_matches(['\n', '\r']).trim().to_owned(),
                words: Vec::new(),
            })
            .collect();
        lines.sort_by_key(|l| l.start_ms);
        Lyrics::from_lines(lines, Origin::EmbeddedSynced)
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use lofty::config::WriteOptions;
    use lofty::id3::v2::{BinaryFrame, FrameId, SyncTextContentType, UnsynchronizedTextFrame};
    use lofty::tag::Accessor as _;
    use lofty::TextEncoding;
    use std::borrow::Cow;
    use std::io::Write as _;

    /// A tiny MP3: the ID3v2 tag, then silent MPEG-1 Layer III frames
    /// (128 kb/s, 44.1 kHz, 417 bytes each) so lofty finds an audio stream.
    fn mp3_with(tag: &Id3v2Tag, dir: &Path) -> std::path::PathBuf {
        let path = dir.join("song.mp3");
        let mut out = Vec::new();
        lofty::tag::TagExt::dump_to(tag, &mut out, WriteOptions::default()).unwrap();
        for _ in 0..8 {
            let mut frame = vec![0u8; 417];
            frame[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
            out.extend_from_slice(&frame);
        }
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&out)
            .unwrap();
        path
    }

    fn sylt_frame(format: TimestampFormat) -> Frame<'static> {
        let sylt = SynchronizedTextFrame::new(
            TextEncoding::UTF8,
            *b"eng",
            format,
            SyncTextContentType::Lyrics,
            None,
            vec![(4_000, "Second".into()), (1_500, "First".into())],
        );
        Frame::Binary(BinaryFrame::new(
            FrameId::Valid(Cow::Borrowed("SYLT")),
            sylt.as_bytes(WriteOptions::default()).unwrap(),
        ))
    }

    fn uslt_frame(text: &str) -> Frame<'static> {
        Frame::UnsynchronizedText(UnsynchronizedTextFrame::new(
            TextEncoding::UTF8,
            *b"eng",
            String::new(),
            text.to_owned(),
        ))
    }

    #[test]
    fn a_sylt_frame_is_synced_lyrics_in_time_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut tag = Id3v2Tag::new();
        tag.set_title("Song".into());
        tag.insert(sylt_frame(TimestampFormat::MS));
        tag.insert(uslt_frame("plain words"));
        let found = read(&mp3_with(&tag, dir.path())).unwrap();
        assert_eq!(found.origin, Origin::EmbeddedSynced);
        let got: Vec<_> = found
            .lines
            .iter()
            .map(|l| (l.start_ms, l.text.as_str()))
            .collect();
        assert_eq!(got, vec![(Some(1_500), "First"), (Some(4_000), "Second")]);
    }

    #[test]
    fn a_uslt_frame_is_plain_lyrics_unless_it_holds_lrc() {
        let dir = tempfile::tempdir().unwrap();
        let mut tag = Id3v2Tag::new();
        tag.insert(uslt_frame("Line one\nLine two"));
        let found = read(&mp3_with(&tag, dir.path())).unwrap();
        assert_eq!(found.origin, Origin::EmbeddedText);
        assert_eq!(found.lines.len(), 2);

        let dir = tempfile::tempdir().unwrap();
        let mut tag = Id3v2Tag::new();
        tag.insert(uslt_frame("[00:01.00]One\n[00:02.00]Two"));
        let found = read(&mp3_with(&tag, dir.path())).unwrap();
        assert_eq!(found.origin, Origin::EmbeddedSynced);
        assert!(found.synced());
    }

    #[test]
    fn a_sylt_frame_timed_in_mpeg_frames_falls_back_to_the_text_tag() {
        let dir = tempfile::tempdir().unwrap();
        let mut tag = Id3v2Tag::new();
        tag.insert(sylt_frame(TimestampFormat::MPEG));
        tag.insert(uslt_frame("plain words"));
        let found = read(&mp3_with(&tag, dir.path())).unwrap();
        assert_eq!(found.origin, Origin::EmbeddedText);
    }

    #[test]
    fn a_file_without_tags_or_that_is_not_audio_has_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = mp3_with(&Id3v2Tag::new(), dir.path());
        assert_eq!(read(&path), None);
        let junk = dir.path().join("junk.flac");
        std::fs::write(&junk, b"not audio").unwrap();
        assert_eq!(read(&junk), None);
        assert_eq!(read(&dir.path().join("missing.mp3")), None);
    }
}
